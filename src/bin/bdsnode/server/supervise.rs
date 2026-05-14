//! Panic isolation for background-task tick bodies.
//!
//! Reliability finding #3: bdsnode's long-lived background tasks
//! (`tokio::spawn(run(...))`) run a `loop { select! { ... } }`.  When
//! a tick body `.await`s work directly — gossip, the rebalancer
//! sweep, the LLM job drain — a panic on that path unwinds the whole
//! `run` task.  The supervising `Handle::stop` only observes the
//! `JoinError` at shutdown, so the subsystem is silently dead until
//! the process restarts.
//!
//! [`tick`] wraps a tick body so a panic is **caught, logged, and
//! swallowed** — the `loop` survives and the next tick runs normally.
//! It uses `catch_unwind` rather than a spawned child task, so the
//! wrapped future may freely borrow loop-local state (`Arc<Cluster>`,
//! `&mut Instant` timers, …) without a `'static` / `Send` bound.
//!
//! `AssertUnwindSafe` is sound here: tick bodies touch
//! `parking_lot` locks (which don't poison) and atomic counters; a
//! panic mid-tick leaves at worst a timer un-reset, which only shifts
//! the next sub-tick by one interval.  Any state that genuinely must
//! survive a panic intact should live behind its own poison-aware
//! lock, not be asserted unwind-safe here.

use futures::future::FutureExt;
use std::panic::AssertUnwindSafe;

/// Run `fut` (a background task's per-tick body) with panic
/// isolation.  Returns `Some(output)` on normal completion, `None`
/// when the body panicked — in which case the panic has already been
/// logged against `task` and the caller's `loop` should simply carry
/// on to the next iteration.
pub async fn tick<F, T>(task: &str, fut: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(v) => Some(v),
        Err(payload) => {
            // Extract a human-readable message from the panic payload
            // when possible — `panic!("msg")` and `.unwrap()` both
            // land here as a `&str` or `String`.
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            log::error!(
                "[{task}] tick panicked ({msg}) — loop survives, next tick unaffected"
            );
            None
        }
    }
}
