//! Background runner for the async LLM job queue.
//!
//! Drains `bdslib::llm::jobs::queue()` on a poll loop: claims pending
//! jobs, dispatches them through `vm::api::llm::{complete,analyze}`
//! (which already handle cache hits + cluster dedup via phases 3+4),
//! pushes the result onto `bdslib::vm::results()` under the job's
//! `result_id`, and marks the row terminal in the queue.
//!
//! Concurrency is bounded by `max_concurrency` (config); the queue
//! itself stays in shape because `claim_one` atomically flips pending
//! → running on a single row.  Cancellation is honoured at two
//! synchronisation points: before the provider call (claim → check
//! cancelled) and after (provider returned → check cancelled).  The
//! provider HTTP call itself is not aborted — the upstream has
//! already incurred whatever cost it incurs.

use bdslib::llm::jobs::{self, Job, JobState};
use serde_json::{json, Value as JsonValue};
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::sync::Semaphore;
use tokio::time::Duration;
use uuid::Uuid;

/// Configuration for the runner.  Pulled from `bds.hjson` at startup.
///
/// | hjson key                       | default | description                                  |
/// |---------------------------------|---------|----------------------------------------------|
/// | `llm.runner.enabled`            | true    | Master switch.                               |
/// | `llm.runner.poll_interval_secs` | 1       | Sleep between claim sweeps when idle.        |
/// | `llm.runner.max_concurrency`    | 2       | Simultaneous in-flight inferences.           |
pub struct Config {
    pub enabled:            bool,
    pub poll_interval_secs: u64,
    pub max_concurrency:    usize,
}

impl Default for Config {
    fn default() -> Self {
        Self { enabled: true, poll_interval_secs: 1, max_concurrency: 2 }
    }
}

impl Config {
    pub fn from_config(config_path: Option<&str>) -> anyhow::Result<Self> {
        let path = match config_path {
            Some(p) => p.to_string(),
            None => match std::env::var("BDS_CONFIG") {
                Ok(p) => p,
                Err(_) => return Ok(Self::default()),
            },
        };
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("cannot read config {path:?}: {e}"))?;
        let val: serde_hjson::Value = serde_hjson::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("hjson parse error in {path:?}: {e}"))?;
        let obj = val.as_object()
            .ok_or_else(|| anyhow::anyhow!("config must be a JSON object"))?;
        let runner = obj.get("llm").and_then(|v| v.as_object())
            .and_then(|llm| llm.get("runner").and_then(|v| v.as_object()));
        let d = Self::default();
        let cfg = match runner {
            Some(r) => Self {
                enabled:            r.get("enabled").and_then(|v| v.as_bool())
                                       .unwrap_or(d.enabled),
                poll_interval_secs: r.get("poll_interval_secs").and_then(|v| v.as_f64())
                                       .map(|n| n as u64).unwrap_or(d.poll_interval_secs)
                                       .max(1),
                max_concurrency:    r.get("max_concurrency").and_then(|v| v.as_f64())
                                       .map(|n| (n as usize).max(1))
                                       .unwrap_or(d.max_concurrency),
            },
            None => d,
        };
        Ok(cfg)
    }
}

pub struct Handle {
    shutdown_tx: oneshot::Sender<()>,
    task:        tokio::task::JoinHandle<()>,
}

impl Handle {
    pub async fn stop(self) {
        let _ = self.shutdown_tx.send(());
        if let Err(e) = self.task.await {
            log::error!("[llm_jobs] task panicked on shutdown: {e:?}");
        }
    }
}

/// Spawn the runner.  When `cfg.enabled` is false the runner task
/// still spawns but immediately observes the disabled flag and exits
/// — keeps the call-site shape uniform with the other server tasks.
pub fn start(cfg: Config) -> Handle {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg, shutdown_rx));
    Handle { shutdown_tx, task }
}

async fn run(cfg: Config, mut shutdown_rx: oneshot::Receiver<()>) {
    if !cfg.enabled {
        log::info!("[llm_jobs] runner disabled (llm.runner.enabled=false)");
        return;
    }
    if jobs::queue().is_none() {
        log::info!("[llm_jobs] runner idle — no job queue initialised");
        return;
    }
    let semaphore = Arc::new(Semaphore::new(cfg.max_concurrency));
    let node_id   = resolve_node_id();
    let poll      = Duration::from_secs(cfg.poll_interval_secs.max(1));
    log::info!(
        "[llm_jobs] runner started (node={node_id} max_concurrency={} poll={}s)",
        cfg.max_concurrency, cfg.poll_interval_secs,
    );

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                log::info!("[llm_jobs] shutdown signal received — stopping");
                break;
            }
            _ = tokio::time::sleep(poll) => {
                // Panic-isolate the claim/drain pass (reliability #3)
                // — a panic in claim_one (DB op on jobs.duckdb) must
                // not kill the runner loop.  Spawned `run_one` tasks
                // are already isolated as their own tasks.
                crate::server::supervise::tick(
                    "llm_jobs",
                    drain_pending(&semaphore, node_id),
                ).await;
            }
        }
    }
}

/// Claim up to `max_concurrency` pending jobs and spawn a task per
/// claim.  Each task releases its semaphore permit when done.
async fn drain_pending(semaphore: &Arc<Semaphore>, node_id: Uuid) {
    let q = match jobs::queue() {
        Some(q) => q,
        None    => return,
    };
    loop {
        let permit = match Arc::clone(semaphore).try_acquire_owned() {
            Ok(p)  => p,
            Err(_) => return,  // saturated; come back next tick
        };
        let claimed = match q.claim_one(node_id) {
            Ok(Some(j)) => j,
            Ok(None)    => return,
            Err(e)      => {
                log::warn!("[llm_jobs] claim_one failed: {e}");
                return;
            }
        };
        // `permit` lives in the task; dropped at end → frees semaphore.
        tokio::spawn(async move {
            let _permit = permit;
            run_one(claimed).await;
        });
    }
}

async fn run_one(job: Job) {
    let job_id    = job.job_id;
    let result_id = job.result_id;
    let kind      = job.kind.clone();
    let req       = job.request_json.clone();

    log::debug!("[llm_jobs] run_one job={job_id} kind={kind} result_id={result_id}");

    // Pre-flight cancellation check.  The operator could have cancelled
    // between submit and claim; `claim_one` may have flipped state to
    // `running` after `cancel` set it to `cancelled` if the cancel
    // landed in the sub-millisecond window — re-check to be safe.
    if is_cancelled(job_id) {
        log::debug!("[llm_jobs] job {job_id} cancelled before provider call — skipping");
        deliver_cancelled_payload(&job);
        return;
    }

    // Dispatch on a blocking thread so the sync helpers can drive
    // their own block_on (provider call goes there via runtime).
    let kind_for_dispatch = kind.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let req_value = bdslib::vm::helpers::eval::json_to_dynamic(req);
        if kind_for_dispatch.starts_with("analyze:") {
            bdslib::vm::api::llm::analyze(req_value)
        } else {
            bdslib::vm::api::llm::complete(req_value)
        }
    }).await;

    // After-provider cancellation check: if the operator hit cancel
    // while we were waiting, we still RAN the inference (the upstream
    // already charged us) but we don't push to ResultQueue and we
    // record `cancelled` instead of `done`.
    if is_cancelled(job_id) {
        log::info!("[llm_jobs] job {job_id} cancelled mid-flight — result discarded");
        deliver_cancelled_payload(&job);
        return;
    }

    let final_payload: JsonValue = match outcome {
        Ok(Ok(value)) => {
            let result_json = bdslib::vm::helpers::eval::dynamic_to_json(value);
            json!({
                "job_id":    job_id.to_string(),
                "result_id": result_id.to_string(),
                "kind":      kind,
                "state":     "done",
                "result":    result_json,
            })
        }
        Ok(Err(e)) => json!({
            "job_id":    job_id.to_string(),
            "result_id": result_id.to_string(),
            "kind":      kind,
            "state":     "failed",
            "error":     format!("{e}"),
        }),
        Err(e) => json!({
            "job_id":    job_id.to_string(),
            "result_id": result_id.to_string(),
            "kind":      kind,
            "state":     "failed",
            "error":     format!("runner task panicked: {e}"),
        }),
    };

    // Deliver the result onto the per-node ResultQueue.  Callers poll
    // v2/results.pull against the job's `result_id` exactly the same
    // way they do for queued Bund script evaluations today.
    bdslib::vm::results().push(result_id, rust_dynamic::value::Value::json(final_payload.clone()));

    // Mark the job row terminal.  Failures here are logged but don't
    // disturb the user — they already have their result via the queue.
    if let Some(q) = jobs::queue() {
        match final_payload.get("state").and_then(|v| v.as_str()) {
            Some("done") => { let _ = q.mark_done(job_id); }
            Some("failed") => {
                let err = final_payload.get("error").and_then(|v| v.as_str()).unwrap_or("");
                let _ = q.mark_failed(job_id, err);
            }
            _ => {}
        }
    }
}

/// Re-check the job row's state.  Returns true when the row is now
/// `cancelled` (the operator stepped in via `v4/llm.jobs.cancel`).
fn is_cancelled(job_id: Uuid) -> bool {
    let q = match jobs::queue() {
        Some(q) => q,
        None    => return false,
    };
    matches!(q.get(job_id), Ok(Some(j)) if j.state == JobState::Cancelled)
}

/// Push a sentinel `cancelled` payload onto the ResultQueue so a
/// polling caller doesn't block forever after a mid-flight cancel.
fn deliver_cancelled_payload(job: &Job) {
    let payload = json!({
        "job_id":    job.job_id.to_string(),
        "result_id": job.result_id.to_string(),
        "kind":      job.kind,
        "state":     "cancelled",
    });
    bdslib::vm::results().push(job.result_id, rust_dynamic::value::Value::json(payload));
}

/// Resolve a stable per-process node id for the runner.  Uses the
/// cluster's node_id when available so the inference_log + jobs.owner
/// columns agree; falls back to a fresh UUIDv7 for standalone runs.
fn resolve_node_id() -> Uuid {
    if let Ok(db) = bdslib::get_db() {
        if let Some(cluster) = db.cluster() {
            return cluster.node_id;
        }
    }
    Uuid::now_v7()
}
