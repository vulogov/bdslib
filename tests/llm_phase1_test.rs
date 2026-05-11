//! Phase-1 tests for `vm::api::llm` helpers.
//!
//! Each test exercises the sync helpers against a process-wide mock
//! provider HTTP server that lives in a dedicated thread for the whole
//! binary.  The mock is initialised exactly once via `OnceLock`, after
//! which the global `ProviderManager` is populated with a single
//! "ollama"-shaped client pointed at the mock.
//!
//! These tests run as `#[test]` (no ambient tokio runtime): `vm::api::llm`
//! goes through `runtime::block_on`, which transparently spins up a
//! fallback runtime when no ambient one is present.

use axum::{routing::post, Json, Router};
use bdslib::llm::manager::{self, ProviderManager};
use bdslib::llm::providers::OllamaProvider;
use bdslib::vm::api::{llm as llm_api, meta};
use bdslib::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use parking_lot::Mutex;
use serde_json::{json, Value as JsonValue};
use std::sync::{Arc, OnceLock};

#[derive(Clone, Default)]
struct MockState {
    /// Last raw JSON body received, per path.
    captured: Arc<Mutex<std::collections::HashMap<String, JsonValue>>>,
}

fn shared_state() -> MockState {
    static S: OnceLock<MockState> = OnceLock::new();
    S.get_or_init(MockState::default).clone()
}

async fn handle_chat(
    axum::extract::State(state): axum::extract::State<MockState>,
    Json(body): Json<JsonValue>,
) -> Json<JsonValue> {
    state.captured.lock().insert("/api/chat".into(), body.clone());
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_owned();
    let last_user = body.get("messages").and_then(|v| v.as_array())
        .and_then(|arr| arr.iter().rev().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user")))
        .and_then(|m| m.get("content").and_then(|c| c.as_str()))
        .unwrap_or("").to_owned();
    Json(json!({
        "model": model,
        "message": {"role": "assistant", "content": format!("echo:{last_user}")},
        "done_reason": "stop",
        "prompt_eval_count": 5,
        "eval_count": 2,
    }))
}

async fn handle_embed(
    axum::extract::State(state): axum::extract::State<MockState>,
    Json(body): Json<JsonValue>,
) -> Json<JsonValue> {
    state.captured.lock().insert("/api/embed".into(), body.clone());
    let texts = body.get("input").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let vectors: Vec<JsonValue> = texts.iter()
        .map(|t| json!([t.as_str().unwrap_or("").len() as f64, 0.0, 1.0]))
        .collect();
    Json(json!({ "embeddings": vectors }))
}

/// Spin the mock server on a background thread with its own
/// `current_thread` runtime, return the URL once it's listening.
fn ensure_mock() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().expect("build mock rt");
            rt.block_on(async move {
                let state = shared_state();
                let app = Router::new()
                    .route("/api/chat",  post(handle_chat))
                    .route("/api/embed", post(handle_embed))
                    .with_state(state);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tx.send(format!("http://{addr}")).unwrap();
                let _ = axum::serve(listener, app).await;
            });
        });
        rx.recv().expect("mock server addr")
    })
}

/// Initialise the process-wide ProviderManager exactly once,
/// registering the mock under the canonical "ollama" name.
fn ensure_manager() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let url = ensure_mock();
        let p   = OllamaProvider::new(url, "llama3.2").unwrap();
        let mut mgr = ProviderManager::empty(Some("ollama".into()));
        mgr.insert("ollama", Arc::new(p));
        manager::init(mgr);
    });
}

/// Mutex that serializes tests inspecting the shared mock-capture map.
/// Tests that *only* check their own return value can run in parallel;
/// tests that touch `shared_state().captured` must hold this lock for
/// the duration of the request + the assertions, since the captured
/// map is keyed by path and concurrent calls overwrite each other.
fn capture_lock() -> std::sync::MutexGuard<'static, ()> {
    static M: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// ─────────────────────────────────────────────────────────────────────

#[test]
fn providers_list_returns_registered_with_capabilities() {
    ensure_manager();
    let v = llm_api::providers_list().expect("providers_list");
    let j = dynamic_to_json(v);

    assert_eq!(j["default"], json!("ollama"));
    let arr = j["providers"].as_array().expect("providers array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], json!("ollama"));
    assert_eq!(arr[0]["default_model"], json!("llama3.2"));
    assert_eq!(arr[0]["capabilities"]["chat"],  json!(true));
    assert_eq!(arr[0]["capabilities"]["embed"], json!(true));
}

#[test]
fn complete_with_prompt_shortcut_round_trips_via_mock() {
    ensure_manager();
    let _g = capture_lock();
    let req = json_to_dynamic(json!({
        "prompt":  "phase-1 hi",
        "options": { "temperature": 0.25, "max_tokens": 32 },
    }));
    let v = llm_api::complete(req).expect("complete");
    let j = dynamic_to_json(v);

    assert_eq!(j["response"], json!("echo:phase-1 hi"));
    assert_eq!(j["provider"], json!("ollama"));
    assert_eq!(j["model"],    json!("llama3.2"));
    assert_eq!(j["finish_reason"], json!("stop"));
    assert_eq!(j["tokens_in"],     json!(5));
    assert_eq!(j["tokens_out"],    json!(2));
    assert!(j["ms"].as_u64().is_some(), "ms should be present");

    // Mock captured the outbound shape: prompt became a single user message,
    // options.num_predict carries the max_tokens.
    let captured = shared_state().captured.lock().get("/api/chat").cloned().unwrap();
    let msgs = captured["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["role"],    json!("user"));
    assert_eq!(msgs[0]["content"], json!("phase-1 hi"));
    assert_eq!(captured["options"]["num_predict"], json!(32));
}

#[test]
fn complete_with_explicit_messages_array() {
    ensure_manager();
    let _g = capture_lock();
    let req = json_to_dynamic(json!({
        "messages": [
            {"role": "system",    "content": "be terse"},
            {"role": "user",      "content": "hello"},
            {"role": "assistant", "content": "hi"},
            {"role": "user",      "content": "again"},
        ],
    }));
    let v = llm_api::complete(req).expect("complete");
    let j = dynamic_to_json(v);
    // Mock echoes the last user message.
    assert_eq!(j["response"], json!("echo:again"));
}

#[test]
fn complete_sets_llm_meta_for_question_mark_meta_word() {
    ensure_manager();
    let _g = capture_lock();
    meta::clear_llm();
    let req = json_to_dynamic(json!({"prompt": "meta-check"}));
    let _ = llm_api::complete(req).expect("complete");

    let m = meta::get_llm().expect("llm meta should be populated");
    assert_eq!(m["provider"],   json!("ollama"));
    assert_eq!(m["model"],      json!("llama3.2"));
    assert_eq!(m["tokens_in"],  json!(5));
    assert_eq!(m["tokens_out"], json!(2));
    assert_eq!(m["cache"],      json!("disabled"));
    assert!(m["ms"].as_u64().is_some());
}

#[test]
fn complete_rejects_request_without_prompt_or_messages() {
    ensure_manager();
    let req = json_to_dynamic(json!({"options": {"max_tokens": 8}}));
    let err = llm_api::complete(req).expect_err("should error");
    let s = format!("{err}");
    assert!(s.contains("prompt") && s.contains("messages"), "got: {s}");
}

#[test]
fn complete_rejects_unknown_provider() {
    ensure_manager();
    let req = json_to_dynamic(json!({"provider": "not-registered", "prompt": "hi"}));
    let err = llm_api::complete(req).expect_err("should error");
    let s = format!("{err}");
    assert!(s.contains("not-registered"), "should mention requested provider: {s}");
}

#[test]
fn embed_with_texts_round_trips_via_mock() {
    ensure_manager();
    let _g = capture_lock();
    let req = json_to_dynamic(json!({
        "model": "nomic-embed",
        "texts": ["alpha", "beta!"],
    }));
    let v = llm_api::embed(req).expect("embed");
    let j = dynamic_to_json(v);

    assert_eq!(j["provider"], json!("ollama"));
    assert_eq!(j["dim"], json!(3));
    let vectors = j["vectors"].as_array().unwrap();
    assert_eq!(vectors.len(), 2);
    // Mock fills slot 0 with text.len(), so "alpha" → 5.0 and "beta!" → 5.0.
    assert_eq!(vectors[0][0], json!(5.0));
    assert_eq!(vectors[1][0], json!(5.0));

    let captured = shared_state().captured.lock().get("/api/embed").cloned().unwrap();
    assert_eq!(captured["model"], json!("nomic-embed"));
    assert_eq!(captured["input"], json!(["alpha", "beta!"]));
}

#[test]
fn embed_text_shortcut_for_single_string() {
    ensure_manager();
    let _g = capture_lock();
    let req = json_to_dynamic(json!({"text": "single"}));
    let v = llm_api::embed(req).expect("embed");
    let j = dynamic_to_json(v);
    assert_eq!(j["vectors"].as_array().unwrap().len(), 1);
}

#[test]
fn embed_rejects_empty_texts() {
    ensure_manager();
    let req = json_to_dynamic(json!({"texts": []}));
    let err = llm_api::embed(req).expect_err("should error");
    assert!(format!("{err}").contains("empty"));
}
