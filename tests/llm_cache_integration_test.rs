//! Phase 3.b — end-to-end cache behaviour through `vm::api::llm::*`.
//!
//! Spins up a mock Ollama-shaped server, plugs an InferenceCache into
//! the process-wide CacheManager, and verifies hit/miss/disabled paths.
//! Counts requests the mock saw to prove the cache short-circuits the
//! provider call on hit.

use axum::{routing::post, Json, Router};
use bdslib::llm::cache::{self as cache, CacheManager, InferenceCache};
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
    /// Bumped on every inbound /api/chat call.
    requests: Arc<Mutex<u64>>,
}

fn shared_state() -> MockState {
    static S: OnceLock<MockState> = OnceLock::new();
    S.get_or_init(MockState::default).clone()
}

async fn handle_chat(
    axum::extract::State(state): axum::extract::State<MockState>,
    Json(body): Json<JsonValue>,
) -> Json<JsonValue> {
    *state.requests.lock() += 1;
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_owned();
    let last_user = body.get("messages").and_then(|v| v.as_array())
        .and_then(|arr| arr.iter().rev().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user")))
        .and_then(|m| m.get("content").and_then(|c| c.as_str()))
        .unwrap_or("").to_owned();
    Json(json!({
        "model": model,
        "message": {"role": "assistant", "content": format!("fresh:{last_user}")},
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

fn ensure_setup() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        // Set up a fresh tempdir for the cache DB (no full bdslib DB
        // needed — complete() doesn't touch ShardsManager).
        let tmp = tempfile::TempDir::new().unwrap();
        let cache_root = tmp.path().join("llm");
        let cache = InferenceCache::open(&cache_root).expect("open cache");
        cache::init(CacheManager::new(cache, true, 3600));
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

fn request_count() -> u64 { *shared_state().requests.lock() }

// ─────────────────────────────────────────────────────────────────────

#[test]
fn complete_first_call_is_a_miss_second_identical_call_is_a_hit() {
    ensure_setup();
    let _g = capture_lock();
    let before = request_count();

    let req1 = json_to_dynamic(json!({"prompt": "cache-this please"}));
    let v1 = llm_api::complete(req1).expect("complete 1");
    let j1 = dynamic_to_json(v1);
    assert_eq!(j1["cache"], json!("miss"));
    assert_eq!(j1["response"], json!("fresh:cache-this please"));

    let after_first = request_count();
    assert_eq!(after_first - before, 1, "miss should hit the provider exactly once");

    // Second call — identical canonical request → cache hit, provider untouched.
    let req2 = json_to_dynamic(json!({"prompt": "cache-this please"}));
    let v2 = llm_api::complete(req2).expect("complete 2");
    let j2 = dynamic_to_json(v2);
    assert_eq!(j2["cache"], json!("hit"));
    assert_eq!(j2["response"], json!("fresh:cache-this please"));
    assert_eq!(j2["ms"], json!(0), "hit should report 0ms");

    let after_second = request_count();
    assert_eq!(after_second, after_first,
        "hit must NOT make a provider call (count unchanged)");

    // llm_meta reflects the hit.
    let m = meta::get_llm().unwrap();
    assert_eq!(m["cache"], json!("hit"));
}

#[test]
fn complete_per_call_cache_false_bypasses_the_cache() {
    ensure_setup();
    let _g = capture_lock();
    let before = request_count();

    // Prime the cache with a known request.
    let _ = llm_api::complete(json_to_dynamic(json!({"prompt": "opt-out target"})))
        .expect("first");
    assert_eq!(request_count() - before, 1);

    // Same canonical request + `cache: false` → forces provider call.
    let v = llm_api::complete(json_to_dynamic(json!({
        "prompt": "opt-out target",
        "cache":  false,
    }))).expect("opt-out");
    let j = dynamic_to_json(v);
    assert_eq!(j["cache"], json!("disabled:opt-out"));
    assert_eq!(request_count() - before, 2,
        "cache:false must make a fresh provider call");
}

#[test]
fn complete_temperature_above_zero_disables_caching() {
    ensure_setup();
    let _g = capture_lock();
    let before = request_count();

    let _ = llm_api::complete(json_to_dynamic(json!({
        "prompt":  "hot-temp",
        "options": { "temperature": 0.7 },
    }))).expect("first temp call");
    // Second call with the same temperature should also miss — even
    // though the canonical request is identical, the temperature gate
    // refuses to cache it.
    let v2 = llm_api::complete(json_to_dynamic(json!({
        "prompt":  "hot-temp",
        "options": { "temperature": 0.7 },
    }))).expect("second temp call");
    let j2 = dynamic_to_json(v2);
    assert_eq!(j2["cache"], json!("disabled:temperature"));

    let calls = request_count() - before;
    assert_eq!(calls, 2,
        "every call with temperature>0 should reach the provider; got {calls}");
}

#[test]
fn complete_temperature_zero_is_cached() {
    ensure_setup();
    let _g = capture_lock();
    let before = request_count();

    let _ = llm_api::complete(json_to_dynamic(json!({
        "prompt":  "deterministic",
        "options": { "temperature": 0.0 },
    }))).expect("first t=0");
    let v2 = llm_api::complete(json_to_dynamic(json!({
        "prompt":  "deterministic",
        "options": { "temperature": 0.0 },
    }))).expect("second t=0");
    let j2 = dynamic_to_json(v2);
    assert_eq!(j2["cache"], json!("hit"));
    assert_eq!(request_count() - before, 1, "second call should NOT hit the provider");
}

#[test]
fn complete_different_models_have_separate_cache_entries() {
    ensure_setup();
    let _g = capture_lock();
    let before = request_count();

    let _ = llm_api::complete(json_to_dynamic(json!({
        "prompt": "same prompt",
        "model":  "llama3.2",
    }))).expect("model A");
    let _ = llm_api::complete(json_to_dynamic(json!({
        "prompt": "same prompt",
        "model":  "mistral",
    }))).expect("model B");

    // Both should miss — different canonical keys due to different models.
    assert_eq!(request_count() - before, 2);

    // Re-running A hits, re-running B hits.
    let _ = llm_api::complete(json_to_dynamic(json!({
        "prompt": "same prompt", "model": "llama3.2",
    }))).unwrap();
    let _ = llm_api::complete(json_to_dynamic(json!({
        "prompt": "same prompt", "model": "mistral",
    }))).unwrap();
    assert_eq!(request_count() - before, 2,
        "both repeated requests should hit cache; count unchanged");
}

#[test]
fn analyze_supplied_is_cacheable_and_repeats_short_circuit() {
    ensure_setup();
    let _g = capture_lock();
    let before = request_count();

    let req = json!({
        "kind":  "supplied",
        "rows":  [{"k": "v"}, {"k": "w"}],
        "query": "summarize",
    });
    let _ = llm_api::analyze(json_to_dynamic(req.clone())).expect("first");
    let v2 = llm_api::analyze(json_to_dynamic(req)).expect("second");
    let j2 = dynamic_to_json(v2);
    assert_eq!(j2["cache"], json!("hit"));
    assert_eq!(request_count() - before, 1);
}

#[test]
fn analyze_cache_key_is_stable_across_row_order() {
    ensure_setup();
    let _g = capture_lock();
    let before = request_count();

    // Same rows, different input order → context::build fingerprints
    // each row independently, vm::api::llm::analyze sorts the
    // fingerprints into the canonical request, so the cache key is
    // identical.
    let req_a = json!({
        "kind":  "supplied",
        "rows":  [{"x": 1}, {"y": 2}],
        "query": "Q",
    });
    let req_b = json!({
        "kind":  "supplied",
        "rows":  [{"y": 2}, {"x": 1}],
        "query": "Q",
    });
    let _ = llm_api::analyze(json_to_dynamic(req_a)).expect("a");
    let v2 = llm_api::analyze(json_to_dynamic(req_b)).expect("b");
    let j2 = dynamic_to_json(v2);
    assert_eq!(j2["cache"], json!("hit"),
        "row order should not change the cache key");
    assert_eq!(request_count() - before, 1);
}
