//! Phase 5.c — end-to-end async job runner.
//!
//! Submits jobs via `vm::api::llm::complete_async`, lets the runner
//! drive them through `vm::api::llm::complete` against a mock provider,
//! then polls `bdslib::vm::results()` under the assigned `result_id` to
//! pick up the final payload.  Verifies the full enqueue → claim →
//! provider → results.push → mark_done loop, plus the cancellation
//! short-circuit and the cluster-meta-style payload shape.
//!
//! Runs under a `multi_thread` tokio runtime so the runner's
//! `tokio::task::spawn_blocking` + inner `block_in_place` path works
//! (the default `#[tokio::test]` is single-thread).

use axum::{routing::post, Json, Router};
use bdslib::llm::cache::{self as cache, CacheManager, InferenceCache};
use bdslib::llm::jobs::{self, JobQueue, JobState};
use bdslib::llm::manager::{self, ProviderManager};
use bdslib::llm::providers::OllamaProvider;
use bdslib::vm::api::llm as llm_api;
use bdslib::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use serde_json::{json, Value as JsonValue};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

// ── Mock provider on a dedicated background runtime ──────────────────

async fn handle_chat(Json(body): Json<JsonValue>) -> Json<JsonValue> {
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_owned();
    let last_user = body.get("messages").and_then(|v| v.as_array())
        .and_then(|arr| arr.iter().rev().find(|m| m["role"] == "user"))
        .and_then(|m| m["content"].as_str())
        .unwrap_or("").to_owned();
    Json(json!({
        "model": model,
        "message": {"role": "assistant", "content": format!("runner:{last_user}")},
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
        // Cache (so the helpers don't bail on the missing manager).
        let tmp = tempfile::TempDir::new().unwrap();
        let cache_root = tmp.path().join("llm");
        let c = InferenceCache::open(&cache_root).expect("open cache");
        cache::init(CacheManager::new(c, true, 3600));

        // JobQueue.
        let qroot = tmp.path().join("llm-jobs");
        let q = JobQueue::open(&qroot).expect("open queue");
        jobs::init(q);

        std::mem::forget(tmp);

        // Provider manager pointed at the mock.
        let url = ensure_mock_url();
        let p   = OllamaProvider::new(url, "llama3.2").unwrap();
        let mut mgr = ProviderManager::empty(Some("ollama".into()));
        mgr.insert("ollama", Arc::new(p));
        manager::init(mgr);
    });
}

/// Serializes the end-to-end runner tests.  They share the
/// process-wide JobQueue + ResultQueue, so a concurrently-running
/// test's runner could claim THIS test's submitted job — making
/// `await_result` time out on the wrong id.  Each test acquires this
/// before submitting; releases happen on drop.
fn runner_lock() -> std::sync::MutexGuard<'static, ()> {
    static M: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

/// Poll the ResultQueue under `result_id` until a value arrives or the
/// deadline expires.  Returns the popped value as JSON.
fn await_result(result_id: Uuid, max_wait: Duration) -> Option<JsonValue> {
    let queues = bdslib::vm::results();
    let deadline = Instant::now() + max_wait;
    while Instant::now() < deadline {
        if let Some(v) = queues.pop(result_id) {
            return Some(bdslib::vm::helpers::eval::dynamic_to_json(v));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_drains_a_submitted_complete_job_and_pushes_to_results() {
    ensure_setup();
    let _g = runner_lock();

    // Boot the runner.  Tight poll interval so the test doesn't take
    // long.  max_concurrency=1 keeps the assertions deterministic.
    let handle = bdsnode_runner_start(1, 1);

    // Submit one job.
    let v = llm_api::complete_async(json_to_dynamic(json!({"prompt": "phase 5.c go"}))).unwrap();
    let j = dynamic_to_json(v);
    let job_id    = Uuid::parse_str(j["job_id"].as_str().unwrap()).unwrap();
    let result_id = Uuid::parse_str(j["result_id"].as_str().unwrap()).unwrap();

    let payload = await_result(result_id, Duration::from_secs(5))
        .expect("runner should have delivered a result");

    assert_eq!(payload["job_id"], json!(job_id.to_string()));
    assert_eq!(payload["kind"],   json!("complete"));
    assert_eq!(payload["state"],  json!("done"));
    let result = &payload["result"];
    assert_eq!(result["response"], json!("runner:phase 5.c go"));
    assert_eq!(result["provider"], json!("ollama"));

    // The jobs table row is now terminal.
    let row = jobs::queue().unwrap().get(job_id).unwrap().unwrap();
    assert_eq!(row.state, JobState::Done);
    assert!(row.finished_at.is_some());

    handle.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_runs_analyze_supplied_jobs_too() {
    ensure_setup();
    let _g = runner_lock();
    let handle = bdsnode_runner_start(1, 1);

    let v = llm_api::analyze_async(json_to_dynamic(json!({
        "kind":  "supplied",
        "rows":  [{"k": "v"}],
        "query": "what",
    }))).unwrap();
    let result_id = Uuid::parse_str(dynamic_to_json(v)["result_id"].as_str().unwrap()).unwrap();

    let payload = await_result(result_id, Duration::from_secs(5))
        .expect("analyze result");
    assert_eq!(payload["state"], json!("done"));
    assert_eq!(payload["kind"],  json!("analyze:supplied"));
    let result = &payload["result"];
    // Through complete() inside analyze: same mock response string.
    assert!(result["response"].as_str().unwrap().starts_with("runner:"));
    assert_eq!(result["kind"], json!("supplied"));

    handle.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_pending_job_makes_runner_deliver_cancelled_payload() {
    ensure_setup();
    let _g = runner_lock();

    // Submit but DON'T start the runner immediately — we want the
    // cancel to land while the job is still pending.
    let v = llm_api::complete_async(json_to_dynamic(json!({"prompt": "cancel-me"}))).unwrap();
    let j = dynamic_to_json(v);
    let job_id    = Uuid::parse_str(j["job_id"].as_str().unwrap()).unwrap();
    let result_id = Uuid::parse_str(j["result_id"].as_str().unwrap()).unwrap();

    // Cancel immediately while still pending.
    let cancel = dynamic_to_json(llm_api::job_cancel(
        json_to_dynamic(json!(job_id.to_string()))).unwrap());
    assert_eq!(cancel["ok"], json!(true));

    // Now start the runner.  It must NOT run the inference (state is
    // cancelled, claim_one filters pending only), but if the runner
    // were started before cancel landed it would have observed the
    // cancellation and pushed the `cancelled` payload.  Verify the
    // queue row is cancelled.
    let handle = bdsnode_runner_start(1, 1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let row = jobs::queue().unwrap().get(job_id).unwrap().unwrap();
    assert_eq!(row.state, JobState::Cancelled);

    // Nothing should land on the ResultQueue for this id — pending jobs
    // that were cancelled before claim never get touched by the runner.
    assert!(bdslib::vm::results().pop(result_id).is_none(),
        "pre-claim cancelled job should NOT push a payload");
    handle.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_records_failure_when_request_is_malformed() {
    ensure_setup();
    let _g = runner_lock();
    let handle = bdsnode_runner_start(1, 1);

    // Submit_async validates, so to test the runner's failure path we
    // need a request that passes basic validation but trips the helper
    // at runtime.  An unregistered provider name does it.
    let v = llm_api::complete_async(json_to_dynamic(json!({
        "prompt":   "force-fail",
        "provider": "nonexistent-provider",
    }))).unwrap();
    let j = dynamic_to_json(v);
    let job_id    = Uuid::parse_str(j["job_id"].as_str().unwrap()).unwrap();
    let result_id = Uuid::parse_str(j["result_id"].as_str().unwrap()).unwrap();

    let payload = await_result(result_id, Duration::from_secs(5))
        .expect("failure should still deliver a payload");
    assert_eq!(payload["state"], json!("failed"));
    assert!(payload["error"].as_str().unwrap().contains("nonexistent-provider"));

    let row = jobs::queue().unwrap().get(job_id).unwrap().unwrap();
    assert_eq!(row.state, JobState::Failed);
    assert!(row.error.is_some());
    handle.stop().await;
}

// ─────────────────────────────────────────────────────────────────────
// Runner adapter — bdsnode's server::llm_jobs::start lives in the bin
// crate, so the test re-creates a small subset by spawning the same
// loop via a hand-rolled task that imports the storage layer directly.
// Kept in sync with the real runner's contract.
// ─────────────────────────────────────────────────────────────────────

struct TestHandle {
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    task:        tokio::task::JoinHandle<()>,
}

impl TestHandle {
    async fn stop(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.task.await;
    }
}

fn bdsnode_runner_start(poll_secs: u64, max_concurrency: usize) -> TestHandle {
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrency));
        let node_id = Uuid::now_v7();
        let poll = Duration::from_millis(50.max(poll_secs * 50));
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => break,
                _ = tokio::time::sleep(poll) => {
                    drain_pending(&semaphore, node_id).await;
                }
            }
        }
    });
    TestHandle { shutdown_tx, task }
}

async fn drain_pending(semaphore: &std::sync::Arc<tokio::sync::Semaphore>, node_id: Uuid) {
    let q = match jobs::queue() {
        Some(q) => q,
        None    => return,
    };
    loop {
        let permit = match std::sync::Arc::clone(semaphore).try_acquire_owned() {
            Ok(p)  => p,
            Err(_) => return,
        };
        let claimed = match q.claim_one(node_id) {
            Ok(Some(j)) => j,
            _ => return,
        };
        tokio::spawn(async move {
            let _permit = permit;
            run_one(claimed).await;
        });
    }
}

async fn run_one(job: bdslib::llm::jobs::Job) {
    let job_id    = job.job_id;
    let result_id = job.result_id;
    let kind      = job.kind.clone();
    let req       = job.request_json.clone();

    if is_cancelled(job_id) {
        push_cancelled(&job);
        return;
    }

    let outcome = tokio::task::spawn_blocking(move || {
        let req_value = bdslib::vm::helpers::eval::json_to_dynamic(req);
        if kind.starts_with("analyze:") {
            bdslib::vm::api::llm::analyze(req_value)
        } else {
            bdslib::vm::api::llm::complete(req_value)
        }
    }).await;

    if is_cancelled(job_id) {
        push_cancelled(&job);
        return;
    }

    let payload: JsonValue = match outcome {
        Ok(Ok(v)) => {
            let r = bdslib::vm::helpers::eval::dynamic_to_json(v);
            json!({"job_id":job_id.to_string(),"result_id":result_id.to_string(),
                   "kind":job.kind,"state":"done","result":r})
        }
        Ok(Err(e)) => json!({"job_id":job_id.to_string(),"result_id":result_id.to_string(),
                             "kind":job.kind,"state":"failed","error":format!("{e}")}),
        Err(e) => json!({"job_id":job_id.to_string(),"result_id":result_id.to_string(),
                         "kind":job.kind,"state":"failed","error":format!("task panic: {e}")}),
    };

    bdslib::vm::results().push(result_id, rust_dynamic::value::Value::json(payload.clone()));
    if let Some(q) = jobs::queue() {
        match payload["state"].as_str() {
            Some("done") => { let _ = q.mark_done(job_id); }
            Some("failed") => {
                let err = payload["error"].as_str().unwrap_or("");
                let _ = q.mark_failed(job_id, err);
            }
            _ => {}
        }
    }
}

fn is_cancelled(job_id: Uuid) -> bool {
    let q = match jobs::queue() { Some(q) => q, None => return false };
    matches!(q.get(job_id), Ok(Some(j)) if j.state == JobState::Cancelled)
}

fn push_cancelled(job: &bdslib::llm::jobs::Job) {
    let p = json!({"job_id":job.job_id.to_string(),"result_id":job.result_id.to_string(),
                   "kind":job.kind,"state":"cancelled"});
    bdslib::vm::results().push(job.result_id, rust_dynamic::value::Value::json(p));
}
