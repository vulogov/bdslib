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

/// Consecutive open failures before the per-shard **circuit breaker**
/// trips Open.  Deliberately lower than [`QUARANTINE_THRESHOLD`] (2 vs
/// 3): the breaker's job is to stop callers *hammering* a struggling
/// shard — each blocked open can cost the full pool-checkout timeout —
/// so it engages one failure *before* quarantine.  The breaker does
/// not replace quarantine: its HalfOpen probes still feed the
/// quarantine tracker, so a genuinely-broken shard still gets
/// quarantined, just without every caller paying the open cost on the
/// way there.
pub const BREAKER_THRESHOLD: u32 = 2;

/// How long the circuit breaker stays **Open** (fast-failing every
/// `shard()` call for the interval) before transitioning to HalfOpen
/// and allowing one probe attempt through.
pub const BREAKER_COOLDOWN_SECS: u64 = 30;

/// Circuit-breaker state for one shard, derived from its `Record`.
///
/// - `Closed` — normal; shard opens are attempted.
/// - `Open` — the breaker tripped and is within its cooldown; every
///   `ShardsCache::shard()` call **fast-fails** instead of paying the
///   (up to 10 s) pool-checkout timeout on a doomed open.
/// - `HalfOpen` — the cooldown has elapsed; the next open attempt is
///   allowed through as a probe.  Its outcome closes the breaker (on
///   success) or re-arms the Open window (on failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

impl BreakerState {
    /// Stable lowercase label for JSON / log output.
    pub fn label(&self) -> &'static str {
        match self {
            BreakerState::Closed   => "closed",
            BreakerState::Open     => "open",
            BreakerState::HalfOpen => "half_open",
        }
    }
}

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
    /// Unix-seconds at which the rebuild healer first declared this
    /// shard *unhealable* (DuckDB itself corrupt — can't rebuild from
    /// local data), or 0 if it has never been unhealable.  Drives the
    /// "FAILED for longer than N" escalation to shard recreation.
    /// Cleared (with the whole `Record`) by [`clear`].
    first_unhealable_ts: AtomicU64,
    /// Open failures since the breaker last closed.  Distinct from
    /// `consecutive` (which drives quarantine) so the two thresholds
    /// can differ.
    breaker_failures: AtomicU32,
    /// Unix-seconds at which the circuit breaker last tripped Open, or
    /// 0 when the breaker is Closed.  The derived [`BreakerState`] is
    /// `Open` while `now - breaker_opened_ts < BREAKER_COOLDOWN_SECS`
    /// and `HalfOpen` once the cooldown has elapsed.
    breaker_opened_ts: AtomicU64,
}

impl Record {
    fn new() -> Self {
        Self {
            consecutive: AtomicU32::new(0),
            last_quarantine_ts: AtomicU64::new(0),
            first_unhealable_ts: AtomicU64::new(0),
            breaker_failures: AtomicU32::new(0),
            breaker_opened_ts: AtomicU64::new(0),
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
    /// Lifetime count of failed shards recreated by the Tier-3
    /// escalation (delete the unrepairable shard + create an empty one
    /// for the rebalancer to repopulate from peers).
    recreations_total: AtomicU64,
    /// Lifetime count of circuit-breaker trips (Closed → Open).
    breaker_trips_total: AtomicU64,
}

impl ShardHealthTracker {
    fn new() -> Self {
        Self {
            records: DashMap::new(),
            quarantines_total: AtomicU64::new(0),
            heals_total: AtomicU64::new(0),
            unhealable_total: AtomicU64::new(0),
            recreations_total: AtomicU64::new(0),
            breaker_trips_total: AtomicU64::new(0),
        }
    }

    /// Record that the rebuild healer repaired a shard.
    pub fn record_heal(&self) {
        self.heals_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Mark a shard as *unhealable* and return the Unix-second
    /// timestamp at which it first entered that state.
    ///
    /// Idempotent across sweeps: the first call for a shard stamps
    /// "now" and bumps `unhealable_total`; later calls return the
    /// original timestamp unchanged.  The healer compares
    /// `now - first_unhealable_ts` against the configured
    /// `failed_shard_recreate_after` window to decide when to escalate
    /// to recreation.  A successful heal or a recreation [`clear`]s the
    /// record, so the next unhealable episode is timed afresh.
    pub fn mark_unhealable(&self, key: ShardKey) -> u64 {
        let now = now_secs();
        let rec = self.records.entry(key).or_insert_with(Record::new);
        // compare_exchange 0 -> now: only the first transition wins, so
        // `unhealable_total` counts distinct episodes, not sweeps.
        match rec.first_unhealable_ts.compare_exchange(
            0, now, Ordering::Relaxed, Ordering::Relaxed,
        ) {
            Ok(_) => {
                self.unhealable_total.fetch_add(1, Ordering::Relaxed);
                now
            }
            Err(existing) => existing,
        }
    }

    /// Record that the healer recreated a failed shard (Tier-3).
    pub fn record_recreation(&self) {
        self.recreations_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Lifetime count of successful shard repairs.
    pub fn heals_total(&self) -> u64 {
        self.heals_total.load(Ordering::Relaxed)
    }

    /// Lifetime count of distinct shards found unrepairable from local data.
    pub fn unhealable_total(&self) -> u64 {
        self.unhealable_total.load(Ordering::Relaxed)
    }

    /// Lifetime count of failed shards recreated by the Tier-3 escalation.
    pub fn recreations_total(&self) -> u64 {
        self.recreations_total.load(Ordering::Relaxed)
    }

    /// Lifetime count of circuit-breaker trips (Closed -> Open).
    pub fn breaker_trips_total(&self) -> u64 {
        self.breaker_trips_total.load(Ordering::Relaxed)
    }

    /// Record a successful shard open.  Resets both the quarantine
    /// consecutive-failure counter **and** the circuit breaker — a
    /// shard that opens cleanly is healthy by definition.
    pub fn record_open_success(&self, key: ShardKey) {
        if let Some(r) = self.records.get(&key) {
            r.consecutive.store(0, Ordering::Relaxed);
            // Close the breaker.
            r.breaker_failures.store(0, Ordering::Relaxed);
            r.breaker_opened_ts.store(0, Ordering::Relaxed);
        }
        // No entry => never failed => nothing to reset.
    }

    /// Record a failed shard open.  Returns `true` when the caller
    /// should quarantine the shard now — i.e. the consecutive-failure
    /// count just crossed [`QUARANTINE_THRESHOLD`] **and** the shard is
    /// not within its post-quarantine cooldown.
    ///
    /// Also drives the **circuit breaker** (independent of, and lower
    /// threshold than, quarantine): the breaker is tracked
    /// unconditionally — even during the quarantine cooldown — so a
    /// shard whose quarantine the healer just cleared but that is
    /// still broken still gets fast-fail protection.
    ///
    /// On a `true` (quarantine) return the consecutive counter is
    /// reset and the cooldown armed, so one threshold crossing yields
    /// exactly one quarantine decision.
    pub fn record_open_failure(&self, key: ShardKey) -> bool {
        let now = now_secs();
        let rec = self.records.entry(key).or_insert_with(Record::new);

        // ── circuit breaker — tracked unconditionally ──────────────
        let bf = rec.breaker_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if bf >= BREAKER_THRESHOLD {
            let opened = rec.breaker_opened_ts.load(Ordering::Relaxed);
            // Trip from Closed, or re-arm from HalfOpen (cooldown
            // elapsed).  A failure inside the Open window is a no-op —
            // the breaker is already fast-failing.
            let half_open_elapsed =
                opened != 0 && now.saturating_sub(opened) >= BREAKER_COOLDOWN_SECS;
            if opened == 0 || half_open_elapsed {
                rec.breaker_opened_ts.store(now, Ordering::Relaxed);
                if opened == 0 {
                    // Distinct trip — count it once.
                    self.breaker_trips_total.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // ── quarantine — cooldown-guarded ──────────────────────────
        // If we quarantined this shard recently, the healer owns it —
        // don't count failures or re-quarantine.
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

    /// The current circuit-breaker state for a shard.
    ///
    /// `ShardsCache::shard()` consults this *before* attempting a
    /// shard open: on [`BreakerState::Open`] it fast-fails instead of
    /// paying the (up to 10 s) pool-checkout timeout on a doomed open.
    /// `Closed` and `HalfOpen` both allow the open attempt through —
    /// `HalfOpen` simply means "the cooldown elapsed, this attempt is
    /// the probe".
    pub fn breaker_check(&self, key: ShardKey) -> BreakerState {
        match self.records.get(&key) {
            None => BreakerState::Closed,
            Some(r) => {
                let opened = r.breaker_opened_ts.load(Ordering::Relaxed);
                if opened == 0 {
                    BreakerState::Closed
                } else if now_secs().saturating_sub(opened) < BREAKER_COOLDOWN_SECS {
                    BreakerState::Open
                } else {
                    BreakerState::HalfOpen
                }
            }
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
    fn mark_unhealable_stamps_once_and_counts_distinct_episodes() {
        let t = fresh();
        let k = (0, 3600);
        let first = t.mark_unhealable(k);
        assert!(first > 0);
        // Repeated calls return the SAME timestamp and don't re-count.
        let again = t.mark_unhealable(k);
        assert_eq!(again, first);
        assert_eq!(t.unhealable_total(), 1, "one distinct episode");
        // clear() resets — the next episode is timed afresh and counted.
        t.clear(k);
        let _ = t.mark_unhealable(k);
        assert_eq!(t.unhealable_total(), 2);
    }

    #[test]
    fn breaker_trips_below_quarantine_threshold() {
        let t = fresh();
        let k = (0, 3600);
        // BREAKER_THRESHOLD (2) < QUARANTINE_THRESHOLD (3): the breaker
        // engages on the 2nd open failure, one before quarantine.
        assert_eq!(t.breaker_check(k), BreakerState::Closed);
        let _ = t.record_open_failure(k);
        assert_eq!(t.breaker_check(k), BreakerState::Closed, "1 failure: still closed");
        let quarantined = t.record_open_failure(k);
        assert!(!quarantined, "2 failures: not yet quarantined");
        assert_eq!(t.breaker_check(k), BreakerState::Open, "2 failures: breaker Open");
        assert_eq!(t.breaker_trips_total(), 1);
    }

    #[test]
    fn breaker_success_closes_it() {
        let t = fresh();
        let k = (0, 3600);
        t.record_open_failure(k);
        t.record_open_failure(k);
        assert_eq!(t.breaker_check(k), BreakerState::Open);
        // A clean open closes the breaker AND resets quarantine count.
        t.record_open_success(k);
        assert_eq!(t.breaker_check(k), BreakerState::Closed);
        assert_eq!(t.consecutive_failures(k), 0);
    }

    #[test]
    fn breaker_trip_does_not_double_count() {
        let t = fresh();
        let k = (0, 3600);
        // Many failures past the threshold — still exactly one trip
        // counted (the breaker is already Open; failures inside the
        // Open window don't re-trip).
        for _ in 0..6 {
            t.record_open_failure(k);
        }
        assert_eq!(t.breaker_trips_total(), 1);
        assert_eq!(t.breaker_check(k), BreakerState::Open);
    }

    #[test]
    fn breaker_distinct_shards_independent() {
        let t = fresh();
        let a = (0, 3600);
        let b = (3600, 7200);
        t.record_open_failure(a);
        t.record_open_failure(a);
        assert_eq!(t.breaker_check(a), BreakerState::Open);
        assert_eq!(t.breaker_check(b), BreakerState::Closed);
    }

    #[test]
    fn key_of_converts_systemtime_intervals() {
        use std::time::Duration;
        let start = UNIX_EPOCH + Duration::from_secs(1_000);
        let end   = UNIX_EPOCH + Duration::from_secs(4_600);
        assert_eq!(key_of(start, end), (1_000, 4_600));
    }
}
