//! Phase-2 integration tests for `vm::api::llm::analyze`.
//!
//! Each ContextSource variant flows through `llm::context::build` and
//! lands at the mock provider; the test asserts on the assembled prompt
//! (the mock captures the last `messages`) and on the `source` block
//! the helper surfaces.
//!
//! Pattern mirrors `llm_chat_test.rs`: dedicated tempdir + init_db,
//! mock provider on a background runtime, capture_lock around tests
//! that inspect shared state.

use axum::{routing::post, Json, Router};
use bdslib::llm::manager::{self, ProviderManager};
use bdslib::llm::providers::OllamaProvider;
use bdslib::vm::api::llm as llm_api;
use bdslib::vm::api::meta;
use bdslib::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use parking_lot::Mutex;
use serde_json::{json, Value as JsonValue};
use std::sync::{Arc, OnceLock};

// ── Mock provider ────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct MockState {
    requests: Arc<Mutex<Vec<JsonValue>>>,
}

fn shared_state() -> MockState {
    static S: OnceLock<MockState> = OnceLock::new();
    S.get_or_init(MockState::default).clone()
}

async fn handle_chat(
    axum::extract::State(state): axum::extract::State<MockState>,
    Json(body): Json<JsonValue>,
) -> Json<JsonValue> {
    state.requests.lock().push(body.clone());
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_owned();
    Json(json!({
        "model": model,
        "message": {"role": "assistant", "content": "analyze-ok"},
        "done_reason": "stop",
        "prompt_eval_count": 9,
        "eval_count": 4,
    }))
}

fn ensure_mock_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().unwrap();
            rt.block_on(async move {
                let state = shared_state();
                let app = Router::new()
                    .route("/api/chat", post(handle_chat))
                    .with_state(state);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tx.send(format!("http://{addr}")).unwrap();
                let _ = axum::serve(listener, app).await;
            });
        });
        rx.recv().unwrap()
    })
}

fn ensure_setup() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("db");
        std::fs::create_dir_all(&db_path).unwrap();
        let cfg_path = tmp.path().join("bds.hjson");
        std::fs::write(&cfg_path, format!(
            "{{\n  dbpath: \"{}\"\n  shard_duration: \"1h\"\n  pool_size: 2\n}}\n",
            db_path.display()
        )).unwrap();
        bdslib::init_db(Some(cfg_path.to_str().unwrap())).expect("init_db");
        std::mem::forget(tmp);

        let url = ensure_mock_url();
        let p   = OllamaProvider::new(url, "llama3.2").unwrap();
        let mut mgr = ProviderManager::empty(Some("ollama".into()));
        mgr.insert("ollama", Arc::new(p));
        manager::init(mgr);
    });
}

fn capture_lock() -> std::sync::MutexGuard<'static, ()> {
    static M: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

/// Pluck the LAST captured request's user message text — that's the
/// composed prompt the helper sent.
fn last_user_message() -> String {
    let reqs = shared_state().requests.lock().clone();
    let last = reqs.last().expect("at least one captured request");
    let msgs = last["messages"].as_array().unwrap();
    msgs.iter().rev().find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .map(str::to_owned)
        .expect("user message present")
}

// ─────────────────────────────────────────────────────────────────────
// Supplied — the simplest variant; no DB dependency
// ─────────────────────────────────────────────────────────────────────

#[test]
fn analyze_with_supplied_rows_fingerprints_and_sends_prompt() {
    ensure_setup();
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    let req = json_to_dynamic(json!({
        "kind":  "supplied",
        "rows":  [
            { "key": "cpu", "value": 92, "host": "node-1" },
            { "key": "mem", "value": 78, "host": "node-2" },
        ],
        "query": "what's hot?",
    }));
    let v = llm_api::analyze(req).expect("analyze");
    let j = dynamic_to_json(v);

    assert_eq!(j["response"], json!("analyze-ok"));
    assert_eq!(j["kind"], json!("supplied"));
    assert_eq!(j["n_rows"], json!(2));
    assert_eq!(j["source"]["kind"], json!("supplied"));
    assert_eq!(j["source"]["row_count"], json!(2));

    let prompt = last_user_message();
    // Default supplied prompt is "Analyze the following supplied rows."
    assert!(prompt.starts_with("Analyze"), "default kind prompt missing: {prompt}");
    // Fingerprinted rows appear in the prompt.
    assert!(prompt.contains("key: cpu"),  "row1 fingerprint missing: {prompt}");
    assert!(prompt.contains("host: node-2"), "row2 fingerprint missing: {prompt}");
    // User's question is appended after the "---" separator.
    assert!(prompt.contains("Question: what's hot?"), "question missing: {prompt}");
}

#[test]
fn analyze_supplied_with_empty_rows_still_asks_the_question() {
    ensure_setup();
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    let req = json_to_dynamic(json!({
        "kind":  "supplied",
        "rows":  [],
        "query": "anything in the data?",
    }));
    let _ = llm_api::analyze(req).expect("analyze");

    let prompt = last_user_message();
    assert!(prompt.contains("anything in the data?"),
        "empty-rows path should still send the question: {prompt}");
    // No `Relevant supplied context` preamble when summary is empty.
    assert!(!prompt.contains("Relevant supplied context"),
        "empty rows should omit the context preamble: {prompt}");
}

#[test]
fn analyze_custom_prompt_template_overrides_default() {
    ensure_setup();
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    let req = json_to_dynamic(json!({
        "kind":            "supplied",
        "rows":            [{"k": "v"}],
        "prompt_template": "EXACT_CUSTOM_PREAMBLE",
        "query":           "go",
    }));
    let _ = llm_api::analyze(req).expect("analyze");

    let prompt = last_user_message();
    assert!(prompt.starts_with("EXACT_CUSTOM_PREAMBLE"),
        "custom template not used: {prompt}");
    // Default per-kind text must NOT also appear.
    assert!(!prompt.contains("Analyze the following supplied rows"),
        "default kind preamble should be suppressed: {prompt}");
}

// ─────────────────────────────────────────────────────────────────────
// Documents — needs the docstore
// ─────────────────────────────────────────────────────────────────────

#[test]
fn analyze_documents_pulls_metadata_and_content_by_id() {
    ensure_setup();
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    let db = bdslib::get_db().unwrap();
    let id = db.doc_add(
        json!({"title": "incident-42", "severity": "high"}),
        b"the database melted at 12:00".as_ref(),
    ).expect("doc_add");

    let req = json_to_dynamic(json!({
        "kind":  "documents",
        "ids":   [id.to_string()],
        "query": "what happened?",
    }));
    let v = llm_api::analyze(req).expect("analyze");
    let j = dynamic_to_json(v);

    assert_eq!(j["kind"], json!("documents"));
    assert_eq!(j["n_rows"], json!(1));
    assert_eq!(j["source"]["row_count"], json!(1));

    let prompt = last_user_message();
    assert!(prompt.contains("title: incident-42"),
        "metadata not fingerprinted: {prompt}");
    assert!(prompt.contains("the database melted"),
        "content not fingerprinted: {prompt}");
}

// ─────────────────────────────────────────────────────────────────────
// Meta + error paths
// ─────────────────────────────────────────────────────────────────────

#[test]
fn analyze_sets_llm_meta_with_kind_and_n_rows() {
    ensure_setup();
    let _g = capture_lock();
    meta::clear_llm();

    let req = json_to_dynamic(json!({
        "kind": "supplied",
        "rows": [{"a": 1}, {"a": 2}, {"a": 3}],
    }));
    let _ = llm_api::analyze(req).expect("analyze");

    let m = meta::get_llm().expect("llm meta should be set");
    assert_eq!(m["kind"],   json!("supplied"));
    assert_eq!(m["n_rows"], json!(3));
    assert_eq!(m["provider"], json!("ollama"));
    assert_eq!(m["cache"],  json!("disabled"));
}

#[test]
fn analyze_rejects_missing_kind() {
    ensure_setup();
    let err = llm_api::analyze(json_to_dynamic(json!({}))).expect_err("should error");
    assert!(format!("{err}").contains("kind"));
}

#[test]
fn analyze_rejects_unknown_kind() {
    ensure_setup();
    let err = llm_api::analyze(json_to_dynamic(json!({"kind": "made-up"})))
        .expect_err("should error");
    let s = format!("{err}");
    assert!(s.contains("made-up"), "should mention the bad kind: {s}");
    assert!(s.contains("supplied") || s.contains("aggregation"),
        "should list the valid kinds: {s}");
}

#[test]
fn analyze_kind_documents_requires_ids() {
    ensure_setup();
    let err = llm_api::analyze(json_to_dynamic(json!({"kind": "documents"})))
        .expect_err("should error");
    assert!(format!("{err}").contains("ids"));
}

#[test]
fn analyze_kind_aggregation_requires_duration() {
    ensure_setup();
    let err = llm_api::analyze(json_to_dynamic(json!({"kind": "aggregation"})))
        .expect_err("should error");
    let s = format!("{err}");
    assert!(s.contains("duration"), "should mention required field: {s}");
}

#[test]
fn analyze_default_prompt_per_kind_differs() {
    ensure_setup();
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    // Run analyze for two kinds and verify each got its own preamble.
    let _ = llm_api::analyze(json_to_dynamic(json!({
        "kind": "supplied", "rows": [{"x": 1}],
    }))).expect("supplied");
    let prompt_supplied = last_user_message();

    // For the second kind use a synthetic "documents" with one valid id
    // so we go through a different default-prompt branch.
    let db = bdslib::get_db().unwrap();
    let id = db.doc_add(json!({"k": "v"}), b"body".as_ref()).unwrap();
    let _ = llm_api::analyze(json_to_dynamic(json!({
        "kind": "documents", "ids": [id.to_string()],
    }))).expect("documents");
    let prompt_documents = last_user_message();

    assert_ne!(prompt_supplied.split('\n').next(), prompt_documents.split('\n').next(),
        "default preambles for different kinds should differ");
    assert!(prompt_supplied.starts_with("Analyze the following supplied"),
        "supplied default missing: {prompt_supplied}");
    assert!(prompt_documents.starts_with("Summarize the key information"),
        "documents default missing: {prompt_documents}");
}
