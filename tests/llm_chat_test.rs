//! Phase-1.b chat-session tests for `llm::chat` + `vm::api::llm::chat`.
//!
//! Needs the global DB (chat history lives in the docstore) plus the
//! ProviderManager pointing at a background mock that mirrors Ollama's
//! `/api/chat`.  Single-test-binary scope; the OnceLocks for DB +
//! manager are populated once per binary.

use axum::{routing::post, Json, Router};
use bdslib::llm::manager::{self, ProviderManager};
use bdslib::llm::providers::OllamaProvider;
use bdslib::llm::{chat as llm_chat, types::CompletionOpts};
use bdslib::vm::api::llm as llm_api;
use bdslib::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use parking_lot::Mutex;
use serde_json::{json, Value as JsonValue};
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

#[derive(Clone, Default)]
struct MockState {
    /// Every chat request the mock has seen, in insertion order.
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
    let last_user = body.get("messages").and_then(|v| v.as_array())
        .and_then(|arr| arr.iter().rev().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user")))
        .and_then(|m| m.get("content").and_then(|c| c.as_str()))
        .unwrap_or("").to_owned();
    Json(json!({
        "model": model,
        "message": {"role": "assistant", "content": format!("reply:{last_user}")},
        "done_reason": "stop",
        "prompt_eval_count": 8,
        "eval_count": 3,
    }))
}

fn ensure_mock_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().expect("build mock rt");
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
        rx.recv().expect("mock server addr")
    })
}

fn ensure_setup() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        // DB first — chat needs the docstore.
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("db");
        std::fs::create_dir_all(&db_path).unwrap();
        let cfg_path = tmp.path().join("bds.hjson");
        std::fs::write(&cfg_path, format!(
            "{{\n  dbpath: \"{}\"\n  shard_duration: \"1h\"\n  pool_size: 2\n}}\n",
            db_path.display()
        )).unwrap();
        bdslib::init_db(Some(cfg_path.to_str().unwrap())).expect("init_db");
        // Leak the tempdir so it survives the whole test binary.
        std::mem::forget(tmp);

        // Then manager pointed at the mock server.
        let url = ensure_mock_url();
        let p   = OllamaProvider::new(url, "llama3.2").unwrap();
        let mut mgr = ProviderManager::empty(Some("ollama".into()));
        mgr.insert("ollama", Arc::new(p));
        manager::init(mgr);
    });
}

/// Serializes tests that inspect the shared `requests` Vec.  Tests that
/// only check their own return values can run in parallel.
fn capture_lock() -> std::sync::MutexGuard<'static, ()> {
    static M: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

// ─────────────────────────────────────────────────────────────────────

#[test]
fn open_and_turn_creates_session_and_records_metadata() {
    ensure_setup();
    let _g = capture_lock();
    let outcome = llm_chat::open_and_turn(
        None,
        None,
        "be concise",
        "hi there",
        CompletionOpts::default(),
    ).expect("open_and_turn");

    assert!(outcome.is_new_session);
    assert_eq!(outcome.response, "reply:hi there");
    assert_eq!(outcome.provider, "ollama");
    assert_eq!(outcome.model,    "llama3.2");

    let md = llm_chat::session_metadata(outcome.chat_id).expect("session_metadata")
        .expect("metadata present");
    assert_eq!(md["provider"],      json!("ollama"));
    assert_eq!(md["model"],         json!("llama3.2"));
    assert_eq!(md["system_prompt"], json!("be concise"));
}

#[test]
fn turn_appends_to_existing_history() {
    ensure_setup();
    let _g = capture_lock();
    let first = llm_chat::open_and_turn(
        None, None,
        "be concise", "first",
        CompletionOpts::default(),
    ).expect("open");
    let chat_id = first.chat_id;
    let second = llm_chat::turn(
        chat_id, "second", None, None, CompletionOpts::default()
    ).expect("turn");
    assert!(!second.is_new_session);
    assert_eq!(second.response, "reply:second");

    // Inspect persisted history: should now have [system, user1, asst1, user2, asst2].
    let db = bdslib::get_db().unwrap();
    let raw = db.doc_get_content(chat_id).unwrap().expect("content present");
    let history: Vec<JsonValue> = serde_json::from_slice(&raw).expect("deserialize");
    assert_eq!(history.len(), 5);
    assert_eq!(history[0]["role"], json!("system"));
    assert_eq!(history[1]["role"], json!("user"));    assert_eq!(history[1]["content"], json!("first"));
    assert_eq!(history[2]["role"], json!("assistant"));
    assert_eq!(history[3]["role"], json!("user"));    assert_eq!(history[3]["content"], json!("second"));
    assert_eq!(history[4]["role"], json!("assistant"));
}

#[test]
fn vm_api_chat_with_chat_id_null_opens_new_session() {
    ensure_setup();
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    let req = json_to_dynamic(json!({
        "message": "from-api",
    }));
    let v = llm_api::chat(req).expect("chat");
    let j = dynamic_to_json(v);

    assert_eq!(j["response"], json!("reply:from-api"));
    assert_eq!(j["is_new_session"], json!(true));
    assert_eq!(j["provider"], json!("ollama"));
    let id = j["chat_id"].as_str().expect("chat_id present");
    let parsed = Uuid::parse_str(id).expect("chat_id is a UUID");

    // Mock saw a single request with [system, user] (system prompt
    // gets seeded by open_and_turn).
    let reqs = shared_state().requests.lock().clone();
    assert_eq!(reqs.len(), 1);
    let msgs = reqs[0]["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], json!("system"));
    assert_eq!(msgs[1]["role"], json!("user"));
    assert_eq!(msgs[1]["content"], json!("from-api"));

    // Session metadata persisted.
    let md = llm_chat::session_metadata(parsed).unwrap().expect("md");
    assert_eq!(md["provider"], json!("ollama"));
}

#[test]
fn vm_api_chat_followup_reuses_session() {
    ensure_setup();
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    let first = llm_api::chat(json_to_dynamic(json!({"message": "alpha"}))).unwrap();
    let chat_id = dynamic_to_json(first)["chat_id"].as_str().unwrap().to_owned();

    let v = llm_api::chat(json_to_dynamic(json!({
        "chat_id": chat_id.clone(),
        "message": "beta",
    }))).unwrap();
    let j = dynamic_to_json(v);
    assert_eq!(j["is_new_session"], json!(false));
    assert_eq!(j["chat_id"], json!(chat_id));
    assert_eq!(j["response"], json!("reply:beta"));

    // The second outbound request must carry the full accumulated history.
    let reqs = shared_state().requests.lock().clone();
    let second = &reqs[1];
    let msgs = second["messages"].as_array().unwrap();
    // [system, user1, asst1, user2]
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[3]["role"], json!("user"));
    assert_eq!(msgs[3]["content"], json!("beta"));
}

#[test]
fn vm_api_chat_with_stale_chat_id_opens_new_session_instead_of_erroring() {
    // Regression: a bdsweb cookie pointing at a session that was wiped
    // (e.g. by `bdsnode --new`) used to surface "session not found" as
    // a hard error.  v4/llm.chat now silently recovers by opening a
    // new session — the response carries the fresh chat_id and the
    // client cookie picks it up on the next turn.
    ensure_setup();
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    // A UUID that DOES NOT exist in the docstore.
    let stale = Uuid::now_v7().to_string();
    let v = llm_api::chat(json_to_dynamic(json!({
        "chat_id": stale.clone(),
        "message": "after a stale cookie",
    }))).expect("recovery, not error");
    let j = dynamic_to_json(v);

    // Auto-recovery → new session id (NOT the stale one) + is_new_session.
    let returned = j["chat_id"].as_str().unwrap();
    assert_ne!(returned, stale,
        "stale chat_id should be replaced by a fresh one, got {returned}");
    assert_eq!(j["is_new_session"], json!(true));
    assert_eq!(j["response"], json!("reply:after a stale cookie"));

    // The new session is real — `session_metadata` finds it.
    let fresh_id = Uuid::parse_str(returned).unwrap();
    assert!(llm_chat::session_metadata(fresh_id).unwrap().is_some());
}

#[test]
fn vm_api_chat_supplied_context_prepends_to_user_message() {
    ensure_setup();
    let _g = capture_lock();
    shared_state().requests.lock().clear();

    let v = llm_api::chat(json_to_dynamic(json!({
        "message": "why is X broken?",
        "context": "row1: cpu spike at 12:00\nrow2: oom at 12:01",
    }))).unwrap();
    let _ = dynamic_to_json(v);

    let reqs = shared_state().requests.lock().clone();
    let msgs = reqs[0]["messages"].as_array().unwrap();
    let user = msgs.iter().find(|m| m["role"] == "user").expect("user msg");
    let content = user["content"].as_str().unwrap();
    assert!(content.contains("cpu spike at 12:00"), "context not embedded: {content}");
    assert!(content.contains("why is X broken?"),   "user question lost: {content}");
}

#[test]
fn vm_api_chat_rejects_missing_message() {
    ensure_setup();
    let err = llm_api::chat(json_to_dynamic(json!({}))).expect_err("should error");
    assert!(format!("{err}").contains("message"));
}
