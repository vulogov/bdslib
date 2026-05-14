//! Per-shard failure tracking and the quarantine decision.
//!
//! Phase 2 of the self-healing work.  A shard is three engines —
//! DuckDB, Tantivy, HNSW — under one directory.  When that directory's
//! state is corrupt (crash mid-write, disk fault), **every** attempt to
//! open the shard fails, and without intervention the failure is
//! permanent: the shard fails forever, taking ingest and search for
//! that time window down with it.
//!
//! This module watches [`crate::shardscache::ShardsCache::shard`]'s
//! open path.  Consecutive open failures for the same shard interval
//! are counted; past [`QUARANTINE_THRESHOLD`] the shard is
//! **quarantined** — flagged in the catalog so `shard()` short-circuits
//! it, keeping the rest of the node serving.  The rebuild healer
//! (`bdsnode/server/shard_healer`) then attempts to repair it.
//!
//! ## Why only open failures
//!
//! Open failure is an *unambiguous* corruption signal — a shard whose
//! `Shard::with_config` returns `Err` genuinely can't be used.
//! Operation-level failures (a search that errors, an `add_batch` that
//! fails) are deliberately **not** tracked here: many are expected
//! (client-malformed queries, transient pool saturation) and
//! conflating them would cause false quarantines.  Subtler engine
//! drift is the consistency sweep's job (a future phase), not this
//! tracker's.
//!
//! ## Cooldown
//!
//! After quarantining a shard, failures for that interval are ignored
//! for [`QUARANTINE_COOLDOWN_SECS`].  This stops a still-broken shard
//! that the healer cleared-then-failed from thrashing the catalog —
//! the healer gets one clean window per cooldown to prove the repair.

use dashmap::DashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Consecutive open failures before a shard is quarantined.  Three is
/// enough to rule out a single transient blip (a momentary pool
/// saturation, a brief fs hiccup) while still reacting fast — at the
/// ingest path's retry cadence this is seconds, not minutes.
pub const QUARANTINE_THRESHOLD: u32 = 3;

/// After quarantining, ignore further failures for this interval for
/// this many seconds.  Gives the rebuild healer a clean window to
/// attempt a repair without the tracker re-quarantining underneath it.
pub const QUARANTINE_COOLDOWN_SECS: u64 = 300;

/// A shard interval — the half-open `[start, end)` Unix-second pair
/// that uniquely identifies a shard within one node's catalog.
pub type ShardKey = (i64, i64);

/// Per-shard failure record.  All fields atomic so the tracker needs
/// no per-key lock.
struct Record {
    /// Consecutive open failures since the last success.  Reset to 0
    /// by [`record_open_success`].
    consecutive: AtomicU32,
    /// Unix-seconds of the most recent quarantine of this shard, or 0
    /// if never quarantined.  Drives the cooldown.
    last_quarantine_ts: AtomicU64,
}

impl Record {
    fn new() -> Self {
        Self {
            consecutive: AtomicU32::new(0),
            last_quarantine_ts: AtomicU64::new(0),
        }
    }
}

/// Process-wide per-shard failure tracker.
pub struct ShardHealthTracker {
    records: DashMap<ShardKey, Record>,
    /// Lifetime count of quarantine decisions — surfaced via
    /// `v2/status` so operators can see healing activity at a glance.
    quarantines_total: AtomicU64,
    /// Lifetime count of shards the rebuild healer successfully
    /// repaired (transient retry OR index rebuild).
    heals_total: AtomicU64,
    /// Lifetime count of quarantined shards the healer could **not**
    /// repair from local data (DuckDB itself corrupt) — these stay
    /// quarantined and need operator / peer-rebuild intervention.
    unhealable_total: AtomicU64,
}

impl ShardHealthTracker {
    fn new() -> Self {
        Self {
            records: DashMap::new(),
            quarantines_total: AtomicU64::new(0),
            heals_total: AtomicU64::new(0),
            unhealable_total: AtomicU64::new(0),
        }
    }

    /// Record that the rebuild healer repaired a shard.
    pub fn record_heal(&self) {
        self.heals_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that the rebuild healer found a shard unrepairable from
    /// local data.
    pub fn record_unhealable(&self) {
        self.unhealable_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Lifetime count of successful shard repairs.
    pub fn heals_total(&self) -> u64 {
        self.heals_total.load(Ordering::Relaxed)
    }

    /// Lifetime count of shards found unrepairable from local data.
    pub fn unhealable_total(&self) -> u64 {
        self.unhealable_total.load(Ordering::Relaxed)
    }

    /// Record a successful shard open.  Resets the consecutive-failure
    /// counter — a shard that opens cleanly is healthy by definition.
    pub fn record_open_success(&self, key: ShardKey) {
        if let Some(r) = self.records.get(&key) {
            r.consecutive.store(0, Ordering::Relaxed);
        }
        // No entry ⇒ never failed ⇒ nothing to reset.
    }

    /// Record a failed shard open.  Returns `true` when the caller
    /// should quarantine the shard now — i.e. the consecutive-failure
    /// count just crossed [`QUARANTINE_THRESHOLD`] **and** the shard is
    /// not within its post-quarantine cooldown.
    ///
    /// On a `true` return the internal counter is reset and the
    /// cooldown timer is armed, so a single threshold crossing yields
    /// exactly one quarantine decision.
    pub fn record_open_failure(&self, key: ShardKey) -> bool {
        let now = now_secs();
        let rec = self.records.entry(key).or_insert_with(Record::new);

        // Cooldown guard: if we quarantined this shard recently, the
        // healer owns it — don't count failures or re-quarantine.
        let last_q = rec.last_quarantine_ts.load(Ordering::Relaxed);
        if last_q != 0 && now.saturating_sub(last_q) < QUARANTINE_COOLDOWN_SECS {
            return false;
        }

        let n = rec.consecutive.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= QUARANTINE_THRESHOLD {
            // Threshold crossed — arm the cooldown, reset the counter
            // (so the next crossing is a fresh THRESHOLD failures), and
            // tell the caller to quarantine.
            rec.consecutive.store(0, Ordering::Relaxed);
            rec.last_quarantine_ts.store(now, Ordering::Relaxed);
            self.quarantines_total.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Clear all tracking state for a shard — called by the rebuild
    /// healer after a successful repair so the shard starts fresh
    /// (counter at 0, cooldown cleared).
    pub fn clear(&self, key: ShardKey) {
        self.records.remove(&key);
    }

    /// Lifetime count of quarantine decisions made by this tracker.
    pub fn quarantines_total(&self) -> u64 {
        self.quarantines_total.load(Ordering::Relaxed)
    }

    /// Current consecutive-failure count for a shard (0 when healthy
    /// or unknown).  Exposed for tests and diagnostics.
    pub fn consecutive_failures(&self, key: ShardKey) -> u32 {
        self.records.get(&key)
            .map(|r| r.consecutive.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

static TRACKER: OnceLock<ShardHealthTracker> = OnceLock::new();

/// Borrow the process-wide shard-health tracker.  Lazily initialised.
pub fn tracker() -> &'static ShardHealthTracker {
    TRACKER.get_or_init(ShardHealthTracker::new)
}

/// Convert a `(SystemTime, SystemTime)` shard interval to the
/// Unix-second [`ShardKey`] the tracker is keyed on.  Times before the
/// epoch clamp to 0 — they can't be valid shard bounds anyway.
pub fn key_of(start: SystemTime, end: SystemTime) -> ShardKey {
    let to_secs = |t: SystemTime| t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (to_secs(start), to_secs(end))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> ShardHealthTracker {
        ShardHealthTracker::new()
    }

    #[test]
    fn below_threshold_does_not_quarantine() {
        let t = fresh();
        let k = (0, 3600);
        for _ in 0..(QUARANTINE_THRESHOLD - 1) {
            assert!(!t.record_open_failure(k));
        }
        assert_eq!(t.consecutive_failures(k), QUARANTINE_THRESHOLD - 1);
        assert_eq!(t.quarantines_total(), 0);
    }

    #[test]
    fn threshold_crossing_quarantines_exactly_once() {
        let t = fresh();
        let k = (0, 3600);
        let mut quarantined = 0;
        for _ in 0..QUARANTINE_THRESHOLD {
            if t.record_open_failure(k) { quarantined += 1; }
        }
        assert_eq!(quarantined, 1, "exactly one quarantine decision");
        assert_eq!(t.quarantines_total(), 1);
        // Counter reset after the decision.
        assert_eq!(t.consecutive_failures(k), 0);
    }

    #[test]
    fn success_resets_the_counter() {
        let t = fresh();
        let k = (0, 3600);
        t.record_open_failure(k);
        t.record_open_failure(k);
        assert_eq!(t.consecutive_failures(k), 2);
        t.record_open_success(k);
        assert_eq!(t.consecutive_failures(k), 0);
        // A single later failure must NOT immediately quarantine —
        // the reset means we start the THRESHOLD count over.
        assert!(!t.record_open_failure(k));
    }

    #[test]
    fn cooldown_suppresses_requarantine() {
        let t = fresh();
        let k = (0, 3600);
        // First crossing → quarantine.
        for _ in 0..QUARANTINE_THRESHOLD {
            t.record_open_failure(k);
        }
        assert_eq!(t.quarantines_total(), 1);
        // Immediately after, more failures are swallowed by the
        // cooldown — no second quarantine, counter stays put.
        for _ in 0..(QUARANTINE_THRESHOLD * 2) {
            assert!(!t.record_open_failure(k));
        }
        assert_eq!(t.quarantines_total(), 1);
    }

    #[test]
    fn clear_resets_everything() {
        let t = fresh();
        let k = (0, 3600);
        t.record_open_failure(k);
        t.record_open_failure(k);
        t.clear(k);
        assert_eq!(t.consecutive_failures(k), 0);
        // After clear, the cooldown is gone too — a fresh threshold
        // crossing quarantines again.
        let mut q = 0;
        for _ in 0..QUARANTINE_THRESHOLD {
            if t.record_open_failure(k) { q += 1; }
        }
        assert_eq!(q, 1);
    }

    #[test]
    fn distinct_shards_track_independently() {
        let t = fresh();
        let a = (0, 3600);
        let b = (3600, 7200);
        t.record_open_failure(a);
        t.record_open_failure(a);
        assert_eq!(t.consecutive_failures(a), 2);
        assert_eq!(t.consecutive_failures(b), 0);
    }

    #[test]
    fn key_of_converts_systemtime_intervals() {
        use std::time::Duration;
        let start = UNIX_EPOCH + Duration::from_secs(1_000);
        let end   = UNIX_EPOCH + Duration::from_secs(4_600);
        assert_eq!(key_of(start, end), (1_000, 4_600));
    }
}
