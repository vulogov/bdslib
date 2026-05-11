//! Per-thread cache of the most-recent v3 fan-out's `cluster_meta`.
//!
//! Bund scripts that go through the `vm::api::*` cluster-aware helpers
//! see only the merged data (the same shape they'd see from a local
//! `db.*` call), keeping the polymorphism transparent.  When a script
//! wants to know whether the last call ran across the cluster — and how
//! many peers answered — it can read this thread-local via the
//! `?cluster.meta` Bund word.
//!
//! The meta is **per-thread**: every cluster-aware helper writes into
//! the calling thread's slot before returning.  Bund VM evaluation is
//! single-threaded per VM instance, so the script always reads the
//! meta from the call it just made.
//!
//! The slot starts empty (`None`) and is also reset to `None` at the
//! start of every cluster-aware helper that doesn't end up running
//! cluster code (e.g. when standalone) — that way a script never
//! confuses stale meta from an earlier call with the current one.

use serde_json::Value as JsonValue;
use std::cell::RefCell;

thread_local! {
    /// Most-recent v3 fan-out result for this thread, as the
    /// `cluster_meta` JSON object that handlers embed.  Shape:
    /// `{enabled: bool, peers_queried: u64, peers_answered: u64,
    ///   partial: bool, failed: [{node_id, url, error}, …]}`.
    static LAST_META: RefCell<Option<JsonValue>> = const { RefCell::new(None) };
}

/// Replace the per-thread cluster_meta cache.  Helpers call this with
/// the `cluster_meta` block produced by `params::v3_cluster_meta`
/// (or an equivalent locally-built one).
pub fn set(meta: JsonValue) {
    LAST_META.with(|cell| *cell.borrow_mut() = Some(meta));
}

/// Clear the per-thread cluster_meta cache.  Helpers call this when
/// they took the standalone path (so the next `?cluster.meta` returns
/// nodata rather than stale info from a previous cluster call on this
/// thread).
pub fn clear() {
    LAST_META.with(|cell| *cell.borrow_mut() = None);
}

/// Snapshot the current per-thread cluster_meta.  Returns `None` when
/// no cluster-aware helper has run on this thread yet, or when the
/// most-recent helper took the standalone path.
pub fn get() -> Option<JsonValue> {
    LAST_META.with(|cell| cell.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_set_get_clear() {
        clear();
        assert!(get().is_none());
        set(json!({"enabled": true, "peers_queried": 2, "peers_answered": 2}));
        let snap = get().unwrap();
        assert_eq!(snap["peers_queried"], 2);
        clear();
        assert!(get().is_none());
    }
}
