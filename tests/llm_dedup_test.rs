//! Phase 4 — dedup integration tests.
//!
//! Standalone-mode behaviour: `vm::api::llm::complete` and `analyze`
//! surface a `dedup` label in the response, but with no cluster
//! `inference_log` available the disposition is always `"disabled"`.
//! The cross-node single-execution behaviour (running on one node,
//! short-circuiting the other via `v2/llm.last_executed`) is exercised
//! in the cluster smoke test binary.
//!
//! Internal lease-guard tests (release_done / release_failed / drop)
//! also live here since they don't need the full runtime — they
//! exercise `InferenceLog` + `InferenceLease` directly.

use axum::{routing::post, Json, Router};
use bdslib::llm::cache::{self as cache, CacheManager, InferenceCache};
use bdslib::llm::dedup::{self, InferenceLease, InferenceLog, InferenceState};
use bdslib::llm::manager::{self, ProviderManager};
use bdslib::llm::providers::OllamaProvider;
use bdslib::vm::api::llm as llm_api;
use bdslib::vm::api::meta;
use bdslib::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use serde_json::{json, Value as JsonValue};
use std::sync::{Arc, OnceLock};
use tempfile::TempDir;
use uuid::Uuid;

// ── Mock provider ────────────────────────────────────────────────────

async fn handle_chat(Json(body): Json<JsonValue>) -> Json<JsonValue> {
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_owned();
    Json(json!({
        "model": model,
        "message": {"role": "assistant", "content": "dedup-ok"},
        "done_reason": "stop",
        "prompt_eval_count": 4,
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
                let app = Router::new().route("/api/chat", post(handle_chat));
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
        let tmp = TempDir::new().unwrap();
        let cache_root = tmp.path().join("llm");
        let cache_db = InferenceCache::open(&cache_root).expect("open cache");
        cache::init(CacheManager::new(cache_db, true, 3600));
        std::mem::forget(tmp);

        // Default dedup settings: enabled, 300s window, 30s wait.  Tests
        // never wait long enough to hit the timeout because there's no
        // running peer to compete with.
        dedup::init_settings(dedup::DedupSettings::default());

        let url = ensure_mock_url();
        let p   = OllamaProvider::new(url, "llama3.2").unwrap();
        let mut mgr = ProviderManager::empty(Some("ollama".into()));
        mgr.insert("ollama", Arc::new(p));
        manager::init(mgr);
    });
}

// ─────────────────────────────────────────────────────────────────────
// Public-helper dedup field — standalone behaviour
// ─────────────────────────────────────────────────────────────────────

#[test]
fn complete_in_standalone_mode_shows_dedup_disabled() {
    ensure_setup();
    let v = llm_api::complete(json_to_dynamic(json!({"prompt": "dedup probe"})))
        .expect("complete");
    let j = dynamic_to_json(v);

    // Standalone (no get_db().cluster()) → dedup is "disabled".
    // First call also misses the cache.
    assert_eq!(j["cache"], json!("miss"));
    assert_eq!(j["dedup"], json!("disabled"),
        "without cluster mode there's nothing to dedup against; got: {}", j["dedup"]);

    // llm_meta carries the same label so ?llm.meta reports it.
    let m = meta::get_llm().unwrap();
    assert_eq!(m["dedup"], json!("disabled"));
}

#[test]
fn analyze_in_standalone_mode_shows_dedup_disabled() {
    ensure_setup();
    let v = llm_api::analyze(json_to_dynamic(json!({
        "kind":  "supplied",
        "rows":  [{"k": "v"}],
        "query": "any?",
    }))).expect("analyze");
    let j = dynamic_to_json(v);
    assert_eq!(j["dedup"], json!("disabled"));
}

#[test]
fn complete_cache_hit_carries_no_dedup_field() {
    ensure_setup();
    // Prime the cache.
    let _ = llm_api::complete(json_to_dynamic(json!({"prompt": "prime-me"})))
        .expect("first");
    // Second identical call → cache hit short-circuits before the
    // dedup gate.  The `dedup` field is NOT added on hit responses
    // because no dedup decision was made.
    let v = llm_api::complete(json_to_dynamic(json!({"prompt": "prime-me"})))
        .expect("hit");
    let j = dynamic_to_json(v);
    assert_eq!(j["cache"], json!("hit"));
    assert!(j.get("dedup").is_none() || j["dedup"] == JsonValue::Null,
        "cache hit should not carry a dedup field: {}", j["dedup"]);
}

// ─────────────────────────────────────────────────────────────────────
// Lease guard semantics — directly against InferenceLog
// ─────────────────────────────────────────────────────────────────────

fn open_log() -> (TempDir, InferenceLog) {
    let tmp = TempDir::new().unwrap();
    let log = InferenceLog::open(tmp.path()).unwrap();
    (tmp, log)
}

#[test]
fn lease_release_done_flips_state_to_done() {
    let (_tmp, log) = open_log();
    let node = Uuid::now_v7();
    log.record_start("k", node, dedup::now_secs()).unwrap();
    let lease = InferenceLease::new(log.clone(), "k".into(), node);
    lease.release_done();
    let row = log.most_recent("k").unwrap().expect("row");
    assert_eq!(row.state, InferenceState::Done);
    assert!(row.finished_at.is_some());
}

#[test]
fn lease_release_failed_flips_state_to_failed() {
    let (_tmp, log) = open_log();
    let node = Uuid::now_v7();
    log.record_start("k", node, dedup::now_secs()).unwrap();
    let lease = InferenceLease::new(log.clone(), "k".into(), node);
    lease.release_failed();
    let row = log.most_recent("k").unwrap().expect("row");
    assert_eq!(row.state, InferenceState::Failed);
}

#[test]
fn lease_dropped_without_release_records_failed() {
    let (_tmp, log) = open_log();
    let node = Uuid::now_v7();
    log.record_start("k", node, dedup::now_secs()).unwrap();
    {
        let _lease = InferenceLease::new(log.clone(), "k".into(), node);
        // dropped here without explicit release — Drop impl records as failed
    }
    let row = log.most_recent("k").unwrap().expect("row");
    assert_eq!(row.state, InferenceState::Failed,
        "dropped lease should mark the running row as failed");
}

// ─────────────────────────────────────────────────────────────────────
// Settings parsing + global accessor
// ─────────────────────────────────────────────────────────────────────

#[test]
fn dedup_settings_global_accessor_returns_default_until_initialised() {
    // Other tests in this binary call ensure_setup() and initialise the
    // OnceLock; this test just verifies the accessor's contract via the
    // (potentially already initialised) global.
    let s = dedup::settings();
    assert!(s.window_secs > 0);
    assert!(s.wait_max_secs > 0);
    // After ensure_setup runs anywhere in this binary, enabled=true.
    ensure_setup();
    assert!(dedup::settings().enabled);
}
