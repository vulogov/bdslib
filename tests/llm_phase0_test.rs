//! Phase-0 integration tests for the LLM provider layer.
//!
//! Each provider is exercised against a small axum mock that mimics the
//! upstream API shape (Ollama `/api/chat` + `/api/embed`, Anthropic
//! `/v1/messages`, OpenAI `/v1/chat/completions` + `/v1/embeddings`).
//! The mocks capture inbound requests so we can assert on the wire
//! format, then return canned responses so we can assert response
//! parsing.  The ProviderManager tests live below and use a tiny
//! in-process `EchoProvider` to exercise the trait/registry contract
//! without any HTTP at all.

use bdslib::llm::manager::{LlmConfig, ProviderManager};
use bdslib::llm::providers::{AnthropicProvider, OllamaProvider, OpenAIProvider, Provider};
use bdslib::llm::types::{
    Capabilities, CompletionOpts, CompletionRequest, CompletionResponse,
    EmbedRequest, Message, Role,
};
use async_trait::async_trait;
use axum::{extract::State, routing::post, Json, Router};
use parking_lot::Mutex;
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

// ── Mock server scaffolding ──────────────────────────────────────────

#[derive(Clone, Default)]
struct MockState {
    /// Last raw JSON body received, per path.
    captured: Arc<Mutex<HashMap<String, JsonValue>>>,
    /// Last headers received, per path.
    headers:  Arc<Mutex<HashMap<String, HashMap<String, String>>>>,
    /// Canned response JSON per path.
    canned:   Arc<Mutex<HashMap<String, JsonValue>>>,
}

impl MockState {
    fn arm(&self, path: &str, response: JsonValue) {
        self.canned.lock().insert(path.to_owned(), response);
    }
    fn captured(&self, path: &str) -> Option<JsonValue> {
        self.captured.lock().get(path).cloned()
    }
    fn header(&self, path: &str, name: &str) -> Option<String> {
        self.headers.lock().get(path).and_then(|m| m.get(name).cloned())
    }
}

async fn record_and_reply(
    State(state):  State<MockState>,
    headers:       axum::http::HeaderMap,
    req:           axum::extract::Request,
) -> Json<JsonValue> {
    let path = req.uri().path().to_owned();
    let body_bytes = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap();
    let body: JsonValue = serde_json::from_slice(&body_bytes)
        .unwrap_or(JsonValue::Null);
    state.captured.lock().insert(path.clone(), body);

    let hm: HashMap<String, String> = headers.iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_lowercase(), s.to_owned())))
        .collect();
    state.headers.lock().insert(path.clone(), hm);

    let canned = state.canned.lock().get(&path).cloned()
        .unwrap_or_else(|| json!({"error": "no canned response armed", "path": path}));
    Json(canned)
}

async fn spawn_mock(routes: &[&str]) -> (String, MockState, tokio::task::JoinHandle<()>) {
    let state = MockState::default();
    let mut router: Router<MockState> = Router::new();
    for r in routes {
        router = router.route(r, post(record_and_reply));
    }
    let app = router.with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { let _ = axum::serve(listener, app).await; });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), state, handle)
}

// ── Ollama ───────────────────────────────────────────────────────────

#[tokio::test]
async fn ollama_provider_completes_against_mock() {
    let (url, state, h) = spawn_mock(&["/api/chat"]).await;
    state.arm("/api/chat", json!({
        "model": "llama3.2",
        "message": {"role": "assistant", "content": "hi from ollama"},
        "done_reason": "stop",
        "prompt_eval_count": 12,
        "eval_count": 7,
    }));

    let p = OllamaProvider::new(&url, "llama3.2").unwrap();
    let resp = p.complete(CompletionRequest {
        model:    "llama3.2".into(),
        messages: vec![Message::system("be brief"), Message::user("hi")],
        options:  CompletionOpts { temperature: Some(0.25), max_tokens: Some(64), ..Default::default() },
    }).await.expect("complete should succeed");

    assert_eq!(resp.text, "hi from ollama");
    assert_eq!(resp.model, "llama3.2");
    assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
    assert_eq!(resp.tokens_in,  Some(12));
    assert_eq!(resp.tokens_out, Some(7));

    let body = state.captured("/api/chat").unwrap();
    assert_eq!(body["model"], json!("llama3.2"));
    assert_eq!(body["stream"], json!(false));
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"],    json!("system"));
    assert_eq!(msgs[0]["content"], json!("be brief"));
    assert_eq!(msgs[1]["role"],    json!("user"));
    // Ollama option fields: temperature -> options.temperature, max_tokens -> options.num_predict.
    // 0.25 is f32-exact so the JSON round-trip stays bit-identical.
    assert_eq!(body["options"]["temperature"], json!(0.25));
    assert_eq!(body["options"]["num_predict"], json!(64));

    h.abort();
}

#[tokio::test]
async fn ollama_provider_embeds_against_mock() {
    let (url, state, h) = spawn_mock(&["/api/embed"]).await;
    state.arm("/api/embed", json!({
        "embeddings": [[0.1, 0.2, 0.3], [0.4, 0.5, 0.6]],
    }));

    let p = OllamaProvider::new(&url, "nomic-embed-text").unwrap();
    let resp = p.embed(EmbedRequest {
        model: "nomic-embed-text".into(),
        texts: vec!["alpha".into(), "beta".into()],
    }).await.expect("embed should succeed");

    assert_eq!(resp.dim, 3);
    assert_eq!(resp.vectors.len(), 2);
    assert_eq!(resp.vectors[0], vec![0.1f32, 0.2, 0.3]);
    assert_eq!(resp.vectors[1], vec![0.4f32, 0.5, 0.6]);

    let body = state.captured("/api/embed").unwrap();
    assert_eq!(body["model"], json!("nomic-embed-text"));
    assert_eq!(body["input"], json!(["alpha", "beta"]));
    h.abort();
}

#[tokio::test]
async fn ollama_capabilities_report_chat_and_embed() {
    let p = OllamaProvider::new("http://localhost:11434", "llama3.2").unwrap();
    assert_eq!(p.id(), "ollama");
    assert_eq!(p.default_model(), "llama3.2");
    let c = p.capabilities();
    assert!(c.chat && c.embed);
}

// ── Anthropic ────────────────────────────────────────────────────────

#[tokio::test]
async fn anthropic_provider_lifts_system_out_of_messages() {
    let (url, state, h) = spawn_mock(&["/v1/messages"]).await;
    state.arm("/v1/messages", json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5",
        "content": [{"type": "text", "text": "from claude"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 30, "output_tokens": 5},
    }));

    let p = AnthropicProvider::new(&url, "test-key", "claude-sonnet-4-5").unwrap();
    let resp = p.complete(CompletionRequest {
        model:    "claude-sonnet-4-5".into(),
        messages: vec![
            Message::system("you are concise"),
            Message::system("respond in english"),  // second system message should be merged
            Message::user("hi"),
        ],
        options:  CompletionOpts { temperature: Some(0.5), max_tokens: Some(256), ..Default::default() },
    }).await.expect("complete should succeed");

    assert_eq!(resp.text, "from claude");
    assert_eq!(resp.finish_reason.as_deref(), Some("end_turn"));
    assert_eq!(resp.tokens_in,  Some(30));
    assert_eq!(resp.tokens_out, Some(5));

    let body = state.captured("/v1/messages").unwrap();
    // System messages must be lifted to the top-level `system` field, merged
    // in order, and NOT included in the `messages` array.
    assert_eq!(body["system"], json!("you are concise\n\nrespond in english"));
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["role"], json!("user"));
    assert_eq!(body["max_tokens"], json!(256));
    assert_eq!(body["temperature"], json!(0.5));
    // Auth + version headers must be present.
    assert_eq!(state.header("/v1/messages", "x-api-key").as_deref(), Some("test-key"));
    assert_eq!(state.header("/v1/messages", "anthropic-version").as_deref(), Some("2023-06-01"));

    h.abort();
}

#[tokio::test]
async fn anthropic_provider_rejects_embed_by_default() {
    let p = AnthropicProvider::new("http://localhost:9", "k", "claude-sonnet-4-5").unwrap();
    assert!(!p.capabilities().embed);
    let err = p.embed(EmbedRequest { model: "x".into(), texts: vec!["a".into()] })
        .await
        .expect_err("anthropic embed should be unsupported");
    let s = format!("{err}");
    assert!(s.contains("anthropic") && s.contains("not support"), "got: {s}");
}

// ── OpenAI ───────────────────────────────────────────────────────────

#[tokio::test]
async fn openai_provider_completes_against_mock() {
    let (url, state, h) = spawn_mock(&["/v1/chat/completions"]).await;
    state.arm("/v1/chat/completions", json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "gpt-4o-mini-2024-07-18",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "openai reply"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14},
    }));

    let p = OpenAIProvider::new(&url, "sk-test", "gpt-4o-mini").unwrap();
    let resp = p.complete(CompletionRequest {
        model:    "gpt-4o-mini".into(),
        messages: vec![Message::user("hi")],
        options:  CompletionOpts { seed: Some(7), max_tokens: Some(32), ..Default::default() },
    }).await.expect("complete should succeed");

    assert_eq!(resp.text, "openai reply");
    // Provider should echo back the model string the server reported.
    assert_eq!(resp.model, "gpt-4o-mini-2024-07-18");
    assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
    assert_eq!(resp.tokens_in,  Some(11));
    assert_eq!(resp.tokens_out, Some(3));

    let body = state.captured("/v1/chat/completions").unwrap();
    assert_eq!(body["model"], json!("gpt-4o-mini"));
    assert_eq!(body["seed"],  json!(7));
    assert_eq!(body["max_tokens"], json!(32));
    assert_eq!(state.header("/v1/chat/completions", "authorization").as_deref(),
               Some("Bearer sk-test"));

    h.abort();
}

#[tokio::test]
async fn openai_provider_embeds_against_mock() {
    let (url, state, h) = spawn_mock(&["/v1/embeddings"]).await;
    state.arm("/v1/embeddings", json!({
        "object": "list",
        "model":  "text-embedding-3-small",
        "data": [
            {"object": "embedding", "embedding": [1.0, 2.0], "index": 0},
            {"object": "embedding", "embedding": [3.0, 4.0], "index": 1},
        ],
    }));

    let p = OpenAIProvider::new(&url, "sk-test", "text-embedding-3-small").unwrap();
    let resp = p.embed(EmbedRequest {
        model: "text-embedding-3-small".into(),
        texts: vec!["a".into(), "b".into()],
    }).await.expect("embed should succeed");

    assert_eq!(resp.dim, 2);
    assert_eq!(resp.vectors, vec![vec![1.0f32, 2.0], vec![3.0f32, 4.0]]);
    h.abort();
}

// ── Manager + config ─────────────────────────────────────────────────

/// In-process Provider impl with no I/O.  Lets the manager tests
/// exercise the trait/registry contract independently of HTTP.
struct EchoProvider { id: String, model: String }

#[async_trait]
impl Provider for EchoProvider {
    fn id(&self) -> &str { &self.id }
    fn default_model(&self) -> &str { &self.model }
    fn capabilities(&self) -> Capabilities { Capabilities { chat: true, embed: false } }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, bdslib::common::error::Error> {
        let last = req.messages.iter().rev().find(|m| m.role == Role::User)
            .map(|m| m.content.clone()).unwrap_or_default();
        Ok(CompletionResponse {
            text: format!("[{}] echo: {}", self.id, last),
            model: req.model,
            finish_reason: Some("stop".into()),
            tokens_in: Some(0), tokens_out: Some(0),
        })
    }
}

fn echo(id: &str, model: &str) -> Arc<dyn Provider> {
    Arc::new(EchoProvider { id: id.into(), model: model.into() })
}

#[tokio::test]
async fn manager_resolve_named_and_default() {
    let mut mgr = ProviderManager::empty(Some("a".into()));
    mgr.insert("a", echo("a", "m1"));
    mgr.insert("b", echo("b", "m2"));

    assert_eq!(mgr.len(), 2);
    assert_eq!(mgr.default_id(), Some("a"));

    let by_name = mgr.resolve(Some("b")).unwrap();
    let resp = by_name.complete(CompletionRequest {
        model: "m2".into(),
        messages: vec![Message::user("hi b")],
        options: CompletionOpts::default(),
    }).await.unwrap();
    assert!(resp.text.starts_with("[b] echo:"), "got: {}", resp.text);

    let by_default = mgr.resolve(None).unwrap();
    assert_eq!(by_default.id(), "a");
}

#[test]
fn manager_unknown_provider_errors_with_registered_list() {
    let mut mgr = ProviderManager::empty(Some("a".into()));
    mgr.insert("a", echo("a", "m1"));
    let s = match mgr.get("nope") {
        Ok(_)  => panic!("expected error"),
        Err(e) => format!("{e}"),
    };
    assert!(s.contains("nope"),    "should mention requested name: {s}");
    assert!(s.contains("\"a\""),   "should list registered names: {s}");
}

#[test]
fn manager_empty_has_no_default() {
    let mgr = ProviderManager::empty(None);
    assert!(mgr.is_empty());
    assert!(mgr.default_id().is_none());
    let s = match mgr.get_default() {
        Ok(_)  => panic!("expected error"),
        Err(e) => format!("{e}"),
    };
    assert!(s.contains("no default provider"), "got: {s}");
}

#[test]
fn llm_config_parses_full_hjson_block() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), r#"{
        llm: {
            default: "anthropic"
            providers: {
                ollama:    { url: "http://ollama:11434", default_model: "mistral" }
                anthropic: { api_key_env: "X_TEST_ANTHROPIC", default_model: "claude-opus-4" }
                openai:    { api_key_env: "X_TEST_OPENAI",    default_model: "gpt-4o" }
            }
        }
    }"#).unwrap();
    let cfg = LlmConfig::load_from_hjson(tmp.path().to_str().unwrap());
    assert_eq!(cfg.default.as_deref(), Some("anthropic"));
    let o = cfg.ollama.unwrap();
    assert_eq!(o.url, "http://ollama:11434");
    assert_eq!(o.default_model, "mistral");
    let a = cfg.anthropic.unwrap();
    assert_eq!(a.api_key_env, "X_TEST_ANTHROPIC");
    assert_eq!(a.default_model, "claude-opus-4");
    let oa = cfg.openai.unwrap();
    assert_eq!(oa.api_key_env, "X_TEST_OPENAI");
    assert_eq!(oa.default_model, "gpt-4o");
}

#[test]
fn llm_config_returns_default_when_file_missing_or_no_llm_block() {
    let cfg = LlmConfig::load_from_hjson("/nonexistent/path/bds.hjson");
    assert!(cfg.default.is_none() && cfg.ollama.is_none()
            && cfg.anthropic.is_none() && cfg.openai.is_none());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), r#"{ unrelated: { foo: "bar" } }"#).unwrap();
    let cfg2 = LlmConfig::load_from_hjson(tmp.path().to_str().unwrap());
    assert!(cfg2.default.is_none() && cfg2.ollama.is_none());
}

#[test]
fn manager_from_config_skips_api_key_providers_when_env_unset() {
    // SAFETY: scoped to a test-only env var name; not racy with other tests
    // because the name is unique to this test.
    unsafe {
        std::env::remove_var("X_PHASE0_UNSET_KEY");
    }
    let cfg = LlmConfig {
        default: Some("ollama".into()),
        ollama: Some(bdslib::llm::manager::OllamaConfig {
            url: "http://localhost:11434".into(),
            default_model: "llama3.2".into(),
        }),
        anthropic: Some(bdslib::llm::manager::AnthropicConfig {
            base_url: "https://api.anthropic.com".into(),
            api_key_env: "X_PHASE0_UNSET_KEY".into(),
            default_model: "claude".into(),
        }),
        openai: None,
        cache: Default::default(),
        dedup: Default::default(),
        chat:  Default::default(),
    };
    let mgr = ProviderManager::from_config(cfg);
    let names = mgr.registered();
    assert_eq!(names, vec!["ollama".to_string()],
        "anthropic should be skipped when its api_key_env is unset");
    assert_eq!(mgr.default_id(), Some("ollama"));
}

#[test]
fn manager_from_config_falls_back_when_default_unregistered() {
    let cfg = LlmConfig {
        default: Some("openai".into()),  // never registered (no env var)
        ollama: Some(bdslib::llm::manager::OllamaConfig {
            url: "http://localhost:11434".into(),
            default_model: "llama3.2".into(),
        }),
        anthropic: None,
        openai: None,
        cache: Default::default(),
        dedup: Default::default(),
        chat:  Default::default(),
    };
    let mgr = ProviderManager::from_config(cfg);
    // ollama is the only registered provider, so resolve(None) must
    // fall back to it even though the config named "openai" as default.
    assert_eq!(mgr.default_id(), Some("ollama"));
    let p = mgr.resolve(None).unwrap();
    assert_eq!(p.id(), "ollama");
}
