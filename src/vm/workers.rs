//! Persistent BUND worker pool backed by the `WORKERS_PIPE` channel.
//!
//! Jobs arrive as `{"id": "<uuidv7>", "code": "<bund script>"}` JSON messages.
//! Each worker thread competes for the next message on the shared channel
//! (natural least-busy dispatch), creates an ephemeral `Bund` VM, executes
//! the script, then drains every value left on the workbench into the global
//! [`crate::vm::RESULTS`] queue under the job's `id`.
//!
//! ## Lifecycle
//!
//! ```no_run
//! use bdslib::vm::workers::BundWorkerPool;
//!
//! BundWorkerPool::start(4).expect("pool start");
//! let id = bdslib::submit_script("42 .").expect("submit");
//! // poll bdslib::vm::results().pop(id) …
//! ```

use bundcore::bundcore::Bund;
use crossbeam::channel::{self, Receiver, Sender};
use easy_error::{Error, err_msg};
use rust_dynamic::value::Value;
use serde_json::Value as JsonValue;
use std::sync::OnceLock;
use std::thread;
use uuid::Uuid;

use crate::vm::helpers::eval::{bund_compile_and_eval, dynamic_to_json};
use crate::vm::vm::init_stdlib;

// ── static pipe ───────────────────────────────────────────────────────────────

/// Sender side of the worker-pool input channel.
///
/// Populated by [`BundWorkerPool::start`].  External code should prefer the
/// [`submit_script`] helper, but direct access is available for embedding the
/// channel in other MPMC topologies.
pub static WORKERS_PIPE: OnceLock<Sender<JsonValue>> = OnceLock::new();

// ── types ─────────────────────────────────────────────────────────────────────

/// A single background worker thread that pulls jobs from the shared receiver.
pub struct BundWorker {
    _handle: thread::JoinHandle<()>,
}

/// A pool of [`BundWorker`] threads sharing one input channel.
///
/// Workers compete for jobs naturally — an idle worker picks up the next
/// pending job, giving least-busy dispatch semantics without explicit tracking.
pub struct BundWorkerPool {
    workers: Vec<BundWorker>,
}

// ── implementation ────────────────────────────────────────────────────────────

impl BundWorkerPool {
    /// Spawn `n_workers` threads and publish the channel sender into
    /// [`WORKERS_PIPE`].  Returns `Err` if called a second time.
    pub fn start(n_workers: usize) -> Result<BundWorkerPool, Error> {
        let (tx, rx) = channel::unbounded::<JsonValue>();
        WORKERS_PIPE
            .set(tx)
            .map_err(|_| err_msg("BundWorkerPool already initialised"))?;

        let workers = (0..n_workers)
            .map(|i| {
                let rx = rx.clone();
                let handle = thread::Builder::new()
                    .name(format!("bund-worker-{i}"))
                    .spawn(move || worker_loop(rx))
                    .expect("bund-worker thread spawn");
                BundWorker { _handle: handle }
            })
            .collect();

        Ok(BundWorkerPool { workers })
    }

    /// Number of worker threads in this pool.
    pub fn n_workers(&self) -> usize {
        self.workers.len()
    }
}

fn worker_loop(rx: Receiver<JsonValue>) {
    while let Ok(msg) = rx.recv() {
        let Some(id_str) = msg.get("id").and_then(|v| v.as_str()) else {
            log::warn!("[bund-worker] message missing 'id' field; skipping");
            continue;
        };
        let id = match Uuid::try_parse(id_str) {
            Ok(u) => u,
            Err(e) => {
                log::warn!("[bund-worker] invalid uuid {id_str:?}: {e}");
                continue;
            }
        };
        let Some(code) = msg.get("code").and_then(|v| v.as_str()) else {
            log::warn!("[bund-worker] message missing 'code' field for id={id}");
            continue;
        };
        let code = code.to_string();

        let mut bund = Bund::new();
        if let Err(e) = init_stdlib(&mut bund) {
            log::error!("[bund-worker] stdlib init failed for id={id}: {e}");
            continue;
        }

        match bund_compile_and_eval(&mut bund.vm, code) {
            Err(e) => log::error!("[bund-worker] eval error for id={id}: {e}"),
            Ok(_) => {
                let results = crate::vm::results();
                while let Some(raw) = bund.vm.stack.pull_from_workbench() {
                    results.push(id, Value::json(dynamic_to_json(raw)));
                }
            }
        }
    }
}

// ── public helper ─────────────────────────────────────────────────────────────

/// Generate a UUIDv7, enqueue `{"id": ..., "code": script}` in the pool,
/// and return the id.
///
/// Poll [`crate::vm::results()`]`.pop(id)` to retrieve results once the
/// worker has finished executing.
///
/// Returns `Err` if [`BundWorkerPool::start`] has not been called.
pub fn submit_script(script: &str) -> Result<Uuid, Error> {
    let tx = WORKERS_PIPE
        .get()
        .ok_or_else(|| err_msg("BundWorkerPool not initialised; call BundWorkerPool::start() first"))?;
    let id = Uuid::now_v7();
    let msg = serde_json::json!({ "id": id.to_string(), "code": script });
    tx.send(msg).map_err(|e| err_msg(e.to_string()))?;
    Ok(id)
}
