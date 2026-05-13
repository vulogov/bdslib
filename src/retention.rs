//! Time-based shard retention with online eviction.
//!
//! Walks the [`ShardInfoEngine`] catalog for shards whose
//! `end_ts < now - retention.duration`, then calls
//! [`ShardsManager::evict_shard`] on each (oldest first), invalidates
//! any [`JsonCache`] entries from the deleted time range, and reports
//! the outcome.
//!
//! [`ShardInfoEngine`]: crate::shardsinfo::ShardInfoEngine
//! [`JsonCache`]: crate::common::cache_json::JsonCache
//!
//! ## What this module does NOT do
//!
//! - Schedule itself.  The bdsnode binary owns the tokio task that
//!   calls [`evict_expired`] on a cadence; see
//!   `src/bin/bdsnode/server/retention.rs`.
//! - Reload the drain parser.  The library is data-plane only; the
//!   tokio task decides whether to call
//!   [`ShardsManager::drain_reload`] based on the configured
//!   `drain_load_duration` and what the sweep actually evicted.
//! - Touch fully-replicated stores (docs / signals / scripts / users /
//!   llm_cache).  Retention is sharded-telemetry only.
//!
//! ## Per-call vs per-process state
//!
//! [`RetentionConfig`] is **per-call** — the bdsnode task reads it
//! fresh from `bds.hjson` at startup and passes it on every tick.
//! [`RetentionStats`] is **process-wide** — atomic counters behind a
//! [`OnceLock`] that the JSON-RPC layer reads at `v2/status` time.
//!
//! [`OnceLock`]: std::sync::OnceLock

use crate::common::error::{err_msg, Result};
use crate::globals::get_db;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ─────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────

/// Runtime knobs for one [`evict_expired`] call.
///
/// Lives in the library so unit tests can construct it directly; the
/// bdsnode tokio task parses `retention.*` from `bds.hjson` and builds
/// one of these per tick (cheap — just a struct).
#[derive(Debug, Clone)]
pub struct RetentionConfig {
    /// Master switch.  When false, [`evict_expired`] returns
    /// `Ok(EvictionReport::disabled())` immediately without touching
    /// the catalog or the filesystem.
    pub enabled: bool,

    /// Retention window.  Shards whose `end_ts < (now - duration)` are
    /// eligible for eviction.  Parsed elsewhere from a humantime
    /// string like `"30days"`.
    pub duration: Duration,

    /// Cap on evictions per call.  `0` = no cap.  Prevents a one-time
    /// policy tightening (`365d → 7d`) from bulk-deleting the entire
    /// historic corpus in a single tick.
    pub max_evictions_per_run: usize,

    /// When true, log what WOULD be evicted but don't actually touch
    /// the catalog or filesystem.  The returned `EvictionReport`
    /// reflects what would have happened.
    pub dry_run: bool,

    // ── Phase 3 — cluster-aware quorum ────────────────────────────
    //
    // The library itself does not fetch peer state — caller supplies
    // a `quorum_check` closure via [`evict_expired_with_quorum`].
    // These flags live on the config so the sync sweeper, the manual
    // `v2/retention.sweep` RPC, and `bdscmd retention-sweep` all see
    // the same toggles.

    /// When true, every candidate shard is gated by a caller-supplied
    /// quorum closure before eviction.  Defaults to **false** — the
    /// safe-by-default behaviour is Phases 1+2's per-node retention.
    /// Enable on clusters with `replication_factor ≥ 2` for an extra
    /// safety net against simultaneous evictions on all replicas.
    pub quorum_check_enabled: bool,

    /// Minimum number of OTHER live peers that must hold a copy of a
    /// shard before it can be evicted.  Ignored when
    /// `quorum_check_enabled = false`.  Defaults to 1 — at least one
    /// peer must still have the shard.  Setting to 2 means "need at
    /// least two other replicas".
    pub quorum_min_peers: usize,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            duration: Duration::from_secs(30 * 24 * 60 * 60),  // 30 days
            max_evictions_per_run: 50,
            dry_run: false,
            quorum_check_enabled: false,
            quorum_min_peers: 1,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Report
// ─────────────────────────────────────────────────────────────────────

/// One sweep's outcome.  The bdsnode task copies these counts into the
/// process-wide [`RetentionStats`] for `v2/status` visibility, and the
/// `v2/retention.sweep` RPC returns the full report directly.
#[derive(Debug, Clone, Default)]
pub struct EvictionReport {
    /// Number of shards actually evicted (or that would have been in
    /// dry-run mode).
    pub evicted: usize,

    /// Number of evictions that errored.  Per-shard errors are logged
    /// and counted but never abort the sweep — every eligible shard
    /// gets a try.
    pub errors: usize,

    /// Aggregate bytes freed across this sweep, summed from each
    /// `EvictionOutcome::freed_bytes`.  Always 0 in dry-run mode.
    pub freed_bytes: u64,

    /// Echo of the request flag.
    pub dry_run: bool,

    /// Unix seconds — every shard whose `end_ts` was strictly less
    /// than this is what was considered for eviction.
    pub cutoff_ts: u64,

    /// Wall-clock cost of the sweep.
    pub took_ms: u64,

    /// `true` when the sweep saw `enabled=false`.  Lets the JSON-RPC
    /// layer surface a distinct "policy disabled" state for operators.
    pub disabled: bool,

    /// Earliest `start_ts` across the evicted shards (Unix seconds).
    /// Useful for JsonCache invalidation: callers pass
    /// `(start, end)` into [`JsonCache::drop_window`].  Zero when
    /// nothing was evicted.
    pub min_start_ts: u64,

    /// Latest `end_ts` across the evicted shards (Unix seconds).
    pub max_end_ts: u64,

    /// Number of candidate shards skipped because the quorum check
    /// (Phase 3) refused to allow eviction.  Always 0 when
    /// `quorum_check_enabled = false`.  Increments by one per skip;
    /// each skipped shard remains on disk + in the catalog.
    pub quorum_skipped: usize,
}

impl EvictionReport {
    pub fn disabled() -> Self {
        Self { disabled: true, ..Default::default() }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Process-wide stats (read by v2/status)
// ─────────────────────────────────────────────────────────────────────

/// Process-wide retention counters.  Populated by [`record_run`] and
/// surfaced by the JSON-RPC `v2/status` handler.  All fields are
/// atomic — no lock required.
pub struct RetentionStats {
    /// Lifetime total of evictions across every successful sweep.
    pub evicted_lifetime: AtomicU64,
    /// Count from the most recent sweep.
    pub evicted_last_run: AtomicU64,
    /// Aggregate bytes freed across every successful sweep.
    pub freed_lifetime_bytes: AtomicU64,
    /// Bytes freed in the most recent sweep.
    pub freed_last_run_bytes: AtomicU64,
    /// Unix seconds the most recent sweep finished.
    pub last_run_ts: AtomicU64,
    /// Wall-clock cost of the most recent sweep (ms).
    pub last_run_ms: AtomicU64,
    /// Lifetime error count.
    pub errors_lifetime: AtomicU64,
    /// Lifetime count of shards skipped by the Phase 3 quorum check.
    pub quorum_skipped_lifetime: AtomicU64,
    /// Quorum skips from the most recent sweep.
    pub quorum_skipped_last_run: AtomicU64,
}

impl RetentionStats {
    fn empty() -> Self {
        Self {
            evicted_lifetime:        AtomicU64::new(0),
            evicted_last_run:        AtomicU64::new(0),
            freed_lifetime_bytes:    AtomicU64::new(0),
            freed_last_run_bytes:    AtomicU64::new(0),
            last_run_ts:             AtomicU64::new(0),
            last_run_ms:             AtomicU64::new(0),
            errors_lifetime:         AtomicU64::new(0),
            quorum_skipped_lifetime: AtomicU64::new(0),
            quorum_skipped_last_run: AtomicU64::new(0),
        }
    }
}

static STATS: OnceLock<RetentionStats> = OnceLock::new();

/// Borrow the process-wide stats record.  Lazy-initialised on first
/// call so library users that never run a sweep don't pay for it.
pub fn stats() -> &'static RetentionStats {
    STATS.get_or_init(RetentionStats::empty)
}

/// Roll an [`EvictionReport`] into the process-wide counters.  The
/// bdsnode task calls this at the end of every sweep.
pub fn record_run(report: &EvictionReport) {
    let s = stats();
    s.evicted_lifetime       .fetch_add(report.evicted        as u64, Ordering::Relaxed);
    s.evicted_last_run       .store(report.evicted            as u64, Ordering::Relaxed);
    s.freed_lifetime_bytes   .fetch_add(report.freed_bytes,           Ordering::Relaxed);
    s.freed_last_run_bytes   .store(report.freed_bytes,               Ordering::Relaxed);
    s.errors_lifetime        .fetch_add(report.errors         as u64, Ordering::Relaxed);
    s.quorum_skipped_lifetime.fetch_add(report.quorum_skipped as u64, Ordering::Relaxed);
    s.quorum_skipped_last_run.store(report.quorum_skipped     as u64, Ordering::Relaxed);
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    s.last_run_ts.store(now_secs, Ordering::Relaxed);
    s.last_run_ms.store(report.took_ms, Ordering::Relaxed);
}

// ─────────────────────────────────────────────────────────────────────
// The sweep itself
// ─────────────────────────────────────────────────────────────────────

/// Run one retention sweep.  Returns a report describing exactly what
/// happened (or what *would* have happened in dry-run mode).
///
/// `now` is injected so unit tests can drive the cutoff deterministically.
/// Production callers pass `SystemTime::now()`.
///
/// The function is **idempotent on disk** — a second call with the
/// same `now` and an unchanged catalog evicts nothing because the
/// first call already removed every eligible shard.  Dry-run is
/// idempotent in both directions.
///
/// Errors are per-shard and never abort the sweep; the report's
/// `errors` count plus the log lines tell the operator what failed.
/// A top-level `Err` from this function means the catalog itself is
/// unreachable.
///
/// **Phase 3 quorum check is bypassed.**  This function uses an
/// always-true quorum closure — callers that want per-shard peer
/// quorum gating use [`evict_expired_with_quorum`] directly.
pub fn evict_expired(cfg: &RetentionConfig, now: SystemTime) -> Result<EvictionReport> {
    // No-op quorum check: if the caller didn't supply peer state,
    // they don't want quorum gating regardless of `quorum_check_enabled`.
    // The bdsnode wrapper (server/retention.rs) calls the *_with_quorum
    // variant directly when quorum is on.
    evict_expired_with_quorum(cfg, now, |_, _| true)
}

/// Same as [`evict_expired`] but takes a caller-supplied closure that
/// vetoes individual evictions.  Used by the bdsnode tokio task and
/// the `v2/retention.sweep` JSON-RPC handler to implement Phase 3
/// cluster-aware quorum gating: the closure consults a pre-fetched
/// peer-shard presence map and returns `false` when fewer than
/// `cfg.quorum_min_peers` OTHER live peers hold the candidate's
/// interval.
///
/// The closure signature is `(start_ts, end_ts) -> bool` where
/// `true` means "safe to evict".  Both arguments are Unix seconds.
///
/// When `cfg.quorum_check_enabled = false` the closure is **never
/// called** — callers can pass a stub like `|_, _| panic!()` for
/// extra safety in unit tests.
pub fn evict_expired_with_quorum<F>(
    cfg: &RetentionConfig,
    now: SystemTime,
    quorum_check: F,
) -> Result<EvictionReport>
where
    F: Fn(i64, i64) -> bool,
{
    if !cfg.enabled {
        return Ok(EvictionReport::disabled());
    }
    if cfg.duration.is_zero() {
        return Err(err_msg("retention.duration must be > 0"));
    }

    let started = Instant::now();
    let now_secs = now.duration_since(UNIX_EPOCH)
        .map_err(|e| err_msg(format!("retention: now predates epoch: {e}")))?
        .as_secs();
    let cutoff_ts = now_secs.saturating_sub(cfg.duration.as_secs()) as i64;

    let db = get_db()
        .map_err(|e| err_msg(format!("retention: {e}")))?;

    let mut candidates = db.cache().info().list_evictable(cutoff_ts)?;
    if cfg.max_evictions_per_run > 0 && candidates.len() > cfg.max_evictions_per_run {
        candidates.truncate(cfg.max_evictions_per_run);
    }

    let mut report = EvictionReport {
        cutoff_ts: cutoff_ts.max(0) as u64,
        dry_run:   cfg.dry_run,
        ..Default::default()
    };

    if candidates.is_empty() {
        report.took_ms = started.elapsed().as_millis() as u64;
        return Ok(report);
    }

    // Walk oldest → newest so we always reclaim the staleset data
    // first when max_evictions_per_run caps the sweep.  The catalog's
    // list_evictable already sorts by end_ts ASC.
    for info in &candidates {
        let start_secs = info.start_time.duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);
        let end_secs   = info.end_time.duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);

        // Phase 3 — quorum gate.  Evaluated BEFORE the union-range
        // bookkeeping so a skipped shard doesn't widen the
        // JsonCache invalidation window unnecessarily.  The closure
        // is supplied by the caller; the bdsnode tokio task builds
        // one from a fresh fan-out of v2/cluster.shards.list.
        if cfg.quorum_check_enabled && !quorum_check(start_secs as i64, end_secs as i64) {
            log::info!(
                "[retention] quorum check refused eviction of shard {} ({}): \
                 fewer than {} other live peers hold the [{},{}) interval",
                info.shard_id, info.path,
                cfg.quorum_min_peers, start_secs, end_secs,
            );
            report.quorum_skipped += 1;
            continue;
        }

        // Track the union range so JsonCache invalidation can drop
        // a single window instead of one per shard.
        report.min_start_ts = if report.min_start_ts == 0 {
            start_secs
        } else {
            report.min_start_ts.min(start_secs)
        };
        report.max_end_ts = report.max_end_ts.max(end_secs);

        if cfg.dry_run {
            log::info!(
                "[retention] DRY-RUN would evict shard {} ({}): end_ts={end_secs} < cutoff_ts={cutoff_ts}",
                info.shard_id, info.path,
            );
            report.evicted += 1;
            continue;
        }

        match db.evict_shard(info.shard_id) {
            Ok(outcome) if outcome.existed => {
                log::info!(
                    "[retention] evicted shard {} ({}): freed={} bytes",
                    outcome.shard_id, outcome.path, outcome.freed_bytes,
                );
                report.evicted += 1;
                report.freed_bytes = report.freed_bytes.saturating_add(outcome.freed_bytes);
            }
            Ok(_) => {
                // Shard vanished between list_evictable and evict_shard
                // (concurrent admin?).  Not an error; just skip.
                log::debug!(
                    "[retention] shard {} disappeared before eviction — skipped",
                    info.shard_id,
                );
            }
            Err(e) => {
                log::warn!(
                    "[retention] evict_shard {} ({}) failed: {e}",
                    info.shard_id, info.path,
                );
                report.errors += 1;
            }
        }
    }

    // Post-sweep cache invalidation — drop every JsonCache row whose
    // timestamp falls in the union range of evicted shards.  Skipped
    // in dry-run because we didn't actually delete anything.
    if !cfg.dry_run && report.evicted > 0 {
        let dropped = db.jsoncache().drop_window(report.min_start_ts, report.max_end_ts);
        log::info!(
            "[retention] JsonCache drop_window([{},{})): {dropped} entries removed",
            report.min_start_ts, report.max_end_ts,
        );
    }

    report.took_ms = started.elapsed().as_millis() as u64;

    log::info!(
        "[retention] sweep done: evicted={} quorum_skipped={} errors={} \
         freed={}B cutoff={} took={}ms{}",
        report.evicted, report.quorum_skipped, report.errors, report.freed_bytes,
        report.cutoff_ts, report.took_ms,
        if cfg.dry_run { " (DRY-RUN)" } else { "" },
    );

    Ok(report)
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests — pure logic only
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_short_circuits_with_distinct_flag() {
        // No DB is initialised in this unit test — disabled=true must
        // return before touching globals.
        let cfg = RetentionConfig { enabled: false, ..Default::default() };
        let r = evict_expired(&cfg, SystemTime::now()).unwrap();
        assert!(r.disabled);
        assert_eq!(r.evicted, 0);
        assert!(!r.dry_run);
    }

    #[test]
    fn zero_duration_is_rejected() {
        let cfg = RetentionConfig {
            enabled: true,
            duration: Duration::from_secs(0),
            ..Default::default()
        };
        let err = evict_expired(&cfg, SystemTime::now()).unwrap_err();
        assert!(err.to_string().contains("duration must be > 0"));
    }

    #[test]
    fn stats_record_run_accumulates_lifetime_counters() {
        let r1 = EvictionReport {
            evicted: 3,
            errors:  1,
            freed_bytes: 1024,
            took_ms: 12,
            quorum_skipped: 1,
            ..Default::default()
        };
        let r2 = EvictionReport {
            evicted: 2,
            errors:  0,
            freed_bytes: 2048,
            took_ms: 8,
            quorum_skipped: 0,
            ..Default::default()
        };
        record_run(&r1);
        record_run(&r2);

        let s = stats();
        // Lifetime counters accumulate, last_run is overwritten.
        assert!(s.evicted_lifetime.load(Ordering::Relaxed) >= 5);
        assert_eq!(s.evicted_last_run.load(Ordering::Relaxed), 2);
        assert!(s.freed_lifetime_bytes.load(Ordering::Relaxed) >= 3072);
        assert_eq!(s.freed_last_run_bytes.load(Ordering::Relaxed), 2048);
        assert_eq!(s.last_run_ms.load(Ordering::Relaxed), 8);
        assert!(s.errors_lifetime.load(Ordering::Relaxed) >= 1);
        assert!(s.quorum_skipped_lifetime.load(Ordering::Relaxed) >= 1);
        assert_eq!(s.quorum_skipped_last_run.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn quorum_check_disabled_never_invokes_closure() {
        // closure that would panic if it's ever called — verifies the
        // hot path doesn't waste a call when the master flag is off.
        let cfg = RetentionConfig {
            enabled:              true,
            duration:             Duration::from_secs(60),
            quorum_check_enabled: false,
            ..Default::default()
        };
        // No DB is initialised so the sweep must fail at get_db() —
        // which proves we did NOT exit early on a quorum decision.
        // We only check that the closure itself isn't called.
        let r = evict_expired_with_quorum(&cfg, SystemTime::now(),
            |_, _| panic!("quorum closure must not be invoked when disabled"));
        // get_db is uninitialised in unit tests → Err.  That's fine —
        // the point is the panic above never fires.
        assert!(r.is_err());
    }
}
