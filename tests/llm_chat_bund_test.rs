//! End-to-end tests for chat-message Bund snippets in `v4/llm.chat`.
//!
//! Exercises three paths through `vm::api::llm::chat`:
//!
//! 1. Snippet detected + globally enabled → eval runs, result spliced
//!    into the prompt as a `\`\`\`json` block, model called normally,
//!    response carries the `bund` stats.
//!
//! 2. Snippet detected + globally enabled + eval ERRORS → no LLM call,
//!    response carries `bund.error` side channel, no chat history
//!    mutation.
//!
//! 3. No snippet + duration set + RAG returns 0 rows → response
//!    carries a `suggest_bund` array with keyword-targeted snippets.
//!
//! Plus a guard test: snippet detected but `chat.bund.enabled=false`
//! → snippet treated as literal text and the standard RAG path runs.
//!
//! Each test runs `#[tokio::test(flavor = "multi_thread")]` because
//! the Bund eval thread is std::thread (no tokio dependency in the
//! eval itself, but the chat helper is sync-from-async via
//! spawn_blocking and the existing per-thread cluster_meta cell only
//! works with a real runtime around it).

use axum::{routing::post, Json, Router};
use bdslib::llm::cache::{self as cache, CacheManager, InferenceCache};
use bdslib::llm::chat_bund::{self, ChatBundSettings, OversizeStrategy};
use bdslib::llm::jobs::{self, JobQueue};
use bdslib::llm::manager::{self, ProviderManager};
use bdslib::llm::providers::OllamaProvider;
use bdslib::llm::snippet::SlashStrictness;
use bdslib::vm::api::llm as llm_api;
use bdslib::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use parking_lot::Mutex;
use serde_json::{json, Value as JsonValue};
use std::sync::{Arc, OnceLock};

#[derive(Clone, Default)]
struct MockState {
    /// Captured request bodies (most-recent first via `.last()`).
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
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("?").to_owned();
    let last_user = body.get("messages").and_then(|v| v.as_array())
        .and_then(|arr| arr.iter().rev().find(|m| m["role"] == "user"))
        .and_then(|m| m["content"].as_str())
        .unwrap_or("").to_owned();
    Json(json!({
        "model": model,
        "message": {"role": "assistant", "content": format!("ack:{last_user}")},
        "done_reason": "stop",
        "prompt_eval_count": 5,
        "eval_count": 2,
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

/// One-time setup: DB (for chat sessions in the docstore), JobQueue,
/// cache + mock provider + ChatBundSettings.
fn ensure_setup(bund_enabled: bool) {
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
        bdslib::init_adam().expect("init_adam");

        // Cache (so disposition resolves cleanly when chat asks).
        let cache_root = tmp.path().join("llm");
        let c = InferenceCache::open(&cache_root).expect("open cache");
        cache::init(CacheManager::new(c, true, 3600));

        // JobQueue (chat doesn't actually use it but llm::chat path
        // does some lookups).
        let qroot = tmp.path().join("llm-jobs");
        let q = JobQueue::open(&qroot).expect("open queue");
        jobs::init(q);

        std::mem::forget(tmp);

        // Settings — picked up via OnceLock.  Per-test override of
        // bund_enabled is done in-test via a wrapper struct held in
        // a Mutex, but since OnceLock is one-shot, we settle on the
        // first call's value here.  Tests below pass bund_enabled=true
        // so the snippet path is exercised; the per-call
        // `bund_enabled: false` override on the request body covers
        // the "globally on but disabled per-call" case.
        chat_bund::init_settings(ChatBundSettings {
            enabled:           bund_enabled,
            timeout_secs:      10,
            max_result_chars:  16384,
            oversize_strategy: OversizeStrategy::Fingerprint,
            slash_strictness:  SlashStrictness::Strict,
            fenced_only:       false,
        });

        // Provider manager pointed at the mock.
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

// ─────────────────────────────────────────────────────────────────────

#[test]
fn snippet_enabled_and_runs_splices_json_into_prompt() {
    ensure_setup(true);
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    let req = json_to_dynamic(json!({
        "message": "```bund\n42\n```\n\nWhat's that result?",
    }));
    let v = llm_api::chat(req).expect("chat");
    let j = dynamic_to_json(v);

    // Response carries the bund stats block with ok=true.
    assert_eq!(j["bund"]["ok"], json!(true));
    assert_eq!(j["bund"]["source"], json!("fenced"));
    let _ms: u64 = j["bund"]["ms"].as_u64().expect("ms present");
    assert!(j["bund"]["code_chars"].as_u64().unwrap() > 0);
    // RAG didn't run (snippet took its place).
    assert_eq!(j["telemetry_count"], json!(0));
    assert_eq!(j["document_count"],  json!(0));

    // Mock received a prompt containing the natural-language remainder.
    let reqs = shared_state().requests.lock().clone();
    let last = reqs.last().expect("captured request");
    let user_msg = last["messages"].as_array().unwrap()
        .iter().rev().find(|m| m["role"] == "user").unwrap()
        ["content"].as_str().unwrap().to_owned();
    assert!(user_msg.contains("Bund snippet result"),
        "prompt should announce the snippet result: {user_msg}");
    assert!(user_msg.contains("What's that result?"),
        "prompt should carry the operator's question: {user_msg}");
}

#[test]
fn snippet_eval_error_returns_side_channel_no_llm_call() {
    ensure_setup(true);
    let _g = capture_lock();
    let requests_before = shared_state().requests.lock().len();

    // Deliberately broken Bund — parser error.
    let req = json_to_dynamic(json!({
        "message": "```bund\n<<<NOT VALID BUND>>>\n```\nfix it",
    }));
    let v = llm_api::chat(req).expect("chat returns Ok even on bund error");
    let j = dynamic_to_json(v);

    assert_eq!(j["bund"]["ok"], json!(false),
        "should report eval failure");
    assert_eq!(j["bund"]["source"], json!("fenced"));
    let err_kind = j["bund"]["error"]["kind"].as_str().unwrap();
    assert!(err_kind == "eval" || err_kind == "stdlib",
        "expected eval/stdlib error kind, got {err_kind}");
    assert!(j["bund"]["error"]["message"].as_str().unwrap().len() > 0,
        "error.message should be non-empty");

    // No model reply produced.
    assert_eq!(j["response"], json!(""));
    assert_eq!(j["is_new_session"], json!(false));

    // Crucial: the LLM was NOT called.
    let requests_after = shared_state().requests.lock().len();
    assert_eq!(requests_after, requests_before,
        "no provider request should have been made on snippet failure");
}

#[test]
fn snippet_detected_but_globally_disabled_falls_through_to_rag() {
    // We can't toggle the global setting per-test (OnceLock), but we
    // CAN exercise the per-call `bund_enabled: false` override which
    // hits the same fall-through code path.
    ensure_setup(true);
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    let req = json_to_dynamic(json!({
        "message":      "```bund\ncls.knn\n```",
        "bund_enabled": false,
    }));
    let v = llm_api::chat(req).expect("chat");
    let j = dynamic_to_json(v);

    // No bund block on the response — we treated the snippet as text.
    assert!(j.get("bund").is_none() || j["bund"].is_null(),
        "bund block should NOT appear when snippet was skipped: {}", j);
    // Model WAS called.
    let reqs = shared_state().requests.lock().clone();
    assert_eq!(reqs.len(), 1, "model should have been called");
    // And the response carries an assistant reply.
    assert!(j["response"].as_str().unwrap().starts_with("ack:"),
        "model reply should be present, got: {}", j["response"]);
}

#[test]
fn empty_rag_with_duration_yields_suggest_bund_recommendations() {
    ensure_setup(true);
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    // No snippet, but a duration is supplied → aggregationsearch
    // will run.  In the smoke setup there's no data ingested, so
    // both counts come back zero and the suggestion path should
    // fire.
    let req = json_to_dynamic(json!({
        "message":  "explain the recent errors",
        "duration": "1h",
    }));
    let v = llm_api::chat(req).expect("chat");
    let j = dynamic_to_json(v);

    assert_eq!(j["telemetry_count"], json!(0));
    assert_eq!(j["document_count"],  json!(0));

    let suggestions = j["suggest_bund"].as_array().expect("suggest_bund present");
    assert!(!suggestions.is_empty(), "expected at least one suggestion");

    // The keyword "errors" should trigger the RCA suggestion + the
    // always-on fallback (knn + inventory).
    let titles: Vec<String> = suggestions.iter()
        .map(|s| s["title"].as_str().unwrap_or("").to_owned()).collect();
    assert!(titles.iter().any(|t| t.contains("Root-cause")),
        "RCA suggestion should fire on 'errors' keyword; titles={titles:?}");
    assert!(titles.iter().any(|t| t.contains("Inventory")),
        "inventory fallback should always appear; titles={titles:?}");

    // Each suggestion has `code` ready to paste — starts with `/`.
    for s in suggestions {
        let code = s["code"].as_str().unwrap();
        assert!(code.starts_with('/'), "code should be /-prefixed for chat input: {code:?}");
    }
}

#[test]
fn empty_rag_without_duration_does_not_suggest() {
    ensure_setup(true);
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    // No duration → no RAG attempted → no suggestion (the
    // suggestion only fires when the operator EXPECTED RAG to
    // produce something).
    let req = json_to_dynamic(json!({
        "message": "explain the recent errors",
    }));
    let v = llm_api::chat(req).expect("chat");
    let j = dynamic_to_json(v);
    assert!(j.get("suggest_bund").is_none() || j["suggest_bund"].is_null(),
        "no duration → no suggestion: {}", j);
}

#[test]
fn snippet_response_carries_no_history_pollution_when_errored() {
    ensure_setup(true);
    let _g = capture_lock();

    // Submit a normal turn first to establish a session.
    let v0 = llm_api::chat(json_to_dynamic(json!({"message": "hello"}))).unwrap();
    let chat_id = dynamic_to_json(v0)["chat_id"].as_str().unwrap().to_owned();

    // Now submit a failing snippet under the SAME chat_id.
    let v = llm_api::chat(json_to_dynamic(json!({
        "chat_id": chat_id.clone(),
        "message": "```bund\n<<<broken>>>\n```",
    }))).expect("chat");
    let j = dynamic_to_json(v);

    // The chat_id should round-trip unchanged — the session is NOT
    // mutated on snippet failure.
    assert_eq!(j["chat_id"], json!(chat_id));
    assert_eq!(j["bund"]["ok"], json!(false));
    assert_eq!(j["response"], json!(""));

    // The docstore session should still have only the system + first
    // user turn + assistant ack from the FIRST call.  No second user
    // turn from the failed snippet.
    let db = bdslib::get_db().unwrap();
    let uid = uuid::Uuid::parse_str(&chat_id).unwrap();
    let raw = db.doc_get_content(uid).unwrap().expect("session present");
    let history: Vec<JsonValue> = serde_json::from_slice(&raw).unwrap();
    let user_turns = history.iter()
        .filter(|m| m["role"] == "user")
        .count();
    assert_eq!(user_turns, 1, "failed snippet must not pollute chat history");
}
