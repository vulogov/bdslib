//! Bridge from synchronous Bund VM code into the async cluster fan-out.
//!
//! The Bund stdlib word handlers are synchronous (`fn(&mut VM) -> Result<…>`),
//! but `bdslib::cluster::fanout::fan_out_v2` is async.  This module exposes
//! a single `block_on(future)` helper that picks the right strategy:
//!
//! - **Inside a tokio runtime** (bdsnode + bdsweb both use
//!   `#[tokio::main]` multi-thread):
//!   `tokio::task::block_in_place` + `Handle::block_on(future)`.
//!   `block_in_place` makes this safe whether we're on a worker thread
//!   or a `spawn_blocking` thread — it temporarily moves the worker so
//!   the runtime can keep making progress while we drive `f` to
//!   completion.
//!
//! - **Outside any tokio runtime** (e.g. `bdscmd` is plain sync):
//!   build (lazily, exactly once per process) a current-thread runtime
//!   and use it for every cluster call.  This means the bdscmd Bund
//!   evaluator can still talk to a remote bdsnode cluster via the
//!   future `vm::api::*` helpers.
//!
//! The helper deliberately panics if `block_in_place` is unavailable
//! (single-thread runtime); that path isn't reachable from any current
//! bdslib binary, and silently fudging it would hide a misconfiguration.

use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::{Handle, Runtime};

/// Lazily-initialised runtime used when the caller is *not* already inside
/// a tokio runtime.  Current-thread is enough because every cluster call
/// is driven serially and we don't expect concurrent fan-outs from the
/// same Bund word.
static FALLBACK_RT: OnceLock<Runtime> = OnceLock::new();

/// Drive `f` to completion from a synchronous context.  See module docs
/// for the strategy used in each environment.
pub fn block_on<F>(f: F) -> F::Output
where
    F: Future,
{
    if let Ok(handle) = Handle::try_current() {
        // We're inside a tokio runtime.  `block_in_place` lets us hand
        // the worker back so the runtime keeps making progress while we
        // park here driving `f`.  Safe on multi-thread runtimes; would
        // panic on single-thread, which we don't use.
        tokio::task::block_in_place(|| handle.block_on(f))
    } else {
        // No ambient runtime — build (or reuse) the fallback.  This is
        // the bdscmd / unit-test code path.
        FALLBACK_RT
            .get_or_init(|| {
                Runtime::new()
                    .expect("vm::api: build fallback tokio runtime")
            })
            .block_on(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `block_on` works with no ambient runtime — the fallback runtime
    /// is created on first use.
    #[test]
    fn block_on_outside_runtime_uses_fallback() {
        let v = block_on(async { 1 + 2 });
        assert_eq!(v, 3);
    }

    /// `block_on` is idempotent across calls (the fallback is reused,
    /// not rebuilt).  Verified indirectly: a second call must not panic
    /// with "runtime already running" or similar.
    #[test]
    fn block_on_outside_runtime_is_repeatable() {
        let _ = block_on(async { 0 });
        let v = block_on(async { 42 });
        assert_eq!(v, 42);
    }
}
