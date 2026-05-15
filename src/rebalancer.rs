//! Background data rebalancer.
//!
//! Per-node tokio task that periodically scans **sharded** telemetry
//! records and assists replication on the cluster.  Distinct from the
//! anti-entropy pull-sync that handles fully-replicated stores
//! (docs/signals/scripts/users/llm_cache) — those converge automatically
//! and don't need this.  Sharded telemetry uses `pick_random_alive`
//! at write time, so a record can end up under-replicated when:
//!
//! - a peer was Suspect/Dead at write time and hints aged out before
//!   it recovered
//! - the cluster expanded after the record was written
//! - replication_factor was raised in `bds.hjson`
//!
//! The rebalancer is **opt-in** (`rebalancer.enabled = false` by
//! default), **non-blocking**, and **cancellable at every atomic
//! boundary**.  It deliberately trades coverage for unobtrusiveness:
//! when ingest is busy (`ingest.lag.p95` above threshold) it skips
//! the tick entirely.  Each per-record push is one atomic write to
//! one peer; cancellation between pushes leaves the cluster in a
//! valid (but partially-rebalanced) state — re-running the task
//! later resumes where it left off.

use crate::common::error::{err_msg, Result};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use uuid::Uuid;

/// 10 minutes — quiet enough that idle clusters don't pay,
/// frequent enough that recovery after a long outage finishes
/// in tens of minutes rather than hours.
pub const DEFAULT_INTERVAL_SECS: u64 = 600;

/// 50 records per `has_records` probe.  Each probe is one
/// fan_out_v2 + at most batch_size individual peer writes.
pub const DEFAULT_BATCH_SIZE: usize = 50;

/// 500 records per tick total.  Caps the work the task can do
/// in one wakeup so the next ingest tick isn't starved.
pub const DEFAULT_MAX_PER_RUN: usize = 500;

/// If `v2/perf.ingest.lag.p95` is above this many ms, skip the
/// tick.  1 second is well above the default `pipe_timeout_ms`
/// (500 ms) — only fires when ingest is genuinely backed up.
pub const DEFAULT_LAG_PAUSE_MS: u64 = 1000;

/// If `v2/perf.ingest.flush.p95` is above this many ms, skip the
/// tick.  Catches the case where the queue is draining (`ingest.lag`
/// is fine) but the actual flush call is taking many seconds because
/// DuckDB / Tantivy / HNSW are saturated — exactly when replicating
/// new records to peers (which goes through the same engines on the
/// receiver) is the worst possible thing the rebalancer can do.
///
/// 5 s default — well above a healthy flush (sub-second on cached
/// shards, a few hundred ms on cold ones), low enough that a node
/// genuinely struggling stops the rebalancer immediately.
pub const DEFAULT_FLUSH_PAUSE_MS: u64 = 5000;

/// TTL of entries in the cross-sweep `(peer, record) → has` cache.
/// Set well above `interval` so back-to-back sweeps reuse each other's
/// answers; the anti-entropy tick is the long-term reconciler so
/// staleness here only delays re-probing, not correctness.
///
/// 30 min default.  0 disables the cache entirely.
pub const DEFAULT_HAVE_CACHE_TTL_SECS: u64 = 1800;

/// Max entries in the have-cache.  100k pairs ≈ 5 MiB at our entry
/// shape, fits comfortably on any node.  0 disables the cache.
pub const DEFAULT_HAVE_CACHE_CAPACITY: usize = 100_000;

/// Number of consecutive probe timeouts within ONE sweep before a
/// peer is dropped from the rest of that sweep.  Reset between
/// sweeps.  0 disables the per-sweep peer-skip behaviour.
pub const DEFAULT_SLOW_PEER_SKIP_AFTER: u32 = 2;

/// Maximum concurrent `v2/cluster.replicate_record` pushes per batch.
/// 4 is a good default — enough to overlap the cold-shard-open cost
/// across peers without saturating the receiver's pool.  1 forces
/// the old serial behaviour (useful when the receiver is fragile).
pub const DEFAULT_PUSH_CONCURRENCY: usize = 4;

/// Operator-tunable knobs.  Parse from the `rebalancer:` block of
/// `bds.hjson` via [`RebalancerConfig::from_hjson_str`].
#[derive(Debug, Clone)]
pub struct RebalancerConfig {
    pub enabled:                 bool,
    pub interval:                Duration,
    pub batch_size:              usize,
    pub max_per_run:             usize,
    /// `None` ⇒ inherit `cluster.replication_factor` at run time.
    pub min_replication_factor:  Option<usize>,
    pub pause_if_ingest_lag_p95_ms: u64,
    /// Skip the tick when `ingest.flush p95` exceeds this many ms.
    /// Sister knob to `pause_if_ingest_lag_p95_ms`; the two measure
    /// different things — lag is queue-wait, flush is actual write
    /// time — and either can independently signal back-pressure.
    pub pause_if_ingest_flush_p95_ms: u64,
    /// TTL of cross-sweep `(peer, record) → has` cache entries.  Long
    /// enough to span typical `interval` values (so back-to-back ticks
    /// reuse each other's answers), short enough to bound staleness on
    /// peer restarts / record deletes.  0 disables the cache.
    pub have_cache_ttl_secs:     u64,
    /// Cap on entries in the have-cache.  0 disables the cache.
    pub have_cache_capacity:     usize,
    /// Within one sweep, a peer that fails this many probe attempts
    /// (timeout / network / rpc error) is skipped for the remainder
    /// of the sweep.  Reset between sweeps.  0 disables peer-skip.
    pub slow_peer_skip_after:    u32,
    /// Max concurrent `v2/cluster.replicate_record` pushes per batch.
    /// 1 forces serial behaviour.
    pub push_concurrency:        usize,
}

impl RebalancerConfig {
    pub fn disabled() -> Self {
        Self {
            enabled:                 false,
            interval:                Duration::from_secs(DEFAULT_INTERVAL_SECS),
            batch_size:              DEFAULT_BATCH_SIZE,
            max_per_run:             DEFAULT_MAX_PER_RUN,
            min_replication_factor:  None,
            pause_if_ingest_lag_p95_ms:   DEFAULT_LAG_PAUSE_MS,
            pause_if_ingest_flush_p95_ms: DEFAULT_FLUSH_PAUSE_MS,
            have_cache_ttl_secs:          DEFAULT_HAVE_CACHE_TTL_SECS,
            have_cache_capacity:          DEFAULT_HAVE_CACHE_CAPACITY,
            slow_peer_skip_after:         DEFAULT_SLOW_PEER_SKIP_AFTER,
            push_concurrency:             DEFAULT_PUSH_CONCURRENCY,
        }
    }

    /// Parse the `rebalancer:` sub-object from a raw hjson string.
    /// Missing block ⇒ disabled.  Invalid keys fall back to defaults
    /// individually.
    pub fn from_hjson_str(raw: &str) -> Result<Self> {
        let val: serde_hjson::Value = serde_hjson::from_str(raw)
            .map_err(|e| err_msg(format!("hjson parse error: {e}")))?;
        let obj = match val.as_object() {
            Some(o) => o,
            None    => return Ok(Self::disabled()),
        };
        let block = match obj.get("rebalancer").and_then(|v| v.as_object()) {
            Some(b) => b,
            None    => return Ok(Self::disabled()),
        };

        let enabled = block.get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !enabled {
            return Ok(Self::disabled());
        }

        let interval = match block.get("interval").and_then(|v| v.as_str()) {
            Some(s) => humantime::parse_duration(s)
                .map_err(|e| err_msg(format!("rebalancer.interval ({s:?}): {e}")))?,
            None => Duration::from_secs(DEFAULT_INTERVAL_SECS),
        };

        let batch_size = block.get("batch_size")
            .and_then(|v| v.as_f64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_BATCH_SIZE)
            .clamp(1, 1000);

        let max_per_run = block.get("max_per_run")
            .and_then(|v| v.as_f64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_PER_RUN);

        let min_replication_factor = block.get("min_replication_factor")
            .and_then(|v| v.as_f64())
            .map(|n| (n as usize).max(1));

        let pause_if_ingest_lag_p95_ms = block.get("pause_if_ingest_lag_p95_ms")
            .and_then(|v| v.as_f64())
            .map(|n| n as u64)
            .unwrap_or(DEFAULT_LAG_PAUSE_MS);

        let pause_if_ingest_flush_p95_ms = block.get("pause_if_ingest_flush_p95_ms")
            .and_then(|v| v.as_f64())
            .map(|n| n as u64)
            .unwrap_or(DEFAULT_FLUSH_PAUSE_MS);

        let have_cache_ttl_secs = block.get("have_cache_ttl_secs")
            .and_then(|v| v.as_f64())
            .map(|n| n as u64)
            .unwrap_or(DEFAULT_HAVE_CACHE_TTL_SECS);

        let have_cache_capacity = block.get("have_cache_capacity")
            .and_then(|v| v.as_f64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_HAVE_CACHE_CAPACITY);

        let slow_peer_skip_after = block.get("slow_peer_skip_after")
            .and_then(|v| v.as_f64())
            .map(|n| n as u32)
            .unwrap_or(DEFAULT_SLOW_PEER_SKIP_AFTER);

        let push_concurrency = block.get("push_concurrency")
            .and_then(|v| v.as_f64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_PUSH_CONCURRENCY)
            .max(1);

        Ok(Self {
            enabled: true,
            interval,
            batch_size,
            max_per_run,
            min_replication_factor,
            pause_if_ingest_lag_p95_ms,
            pause_if_ingest_flush_p95_ms,
            have_cache_ttl_secs,
            have_cache_capacity,
            slow_peer_skip_after,
            push_concurrency,
        })
    }

    pub fn from_path(config_path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(config_path)
            .map_err(|e| err_msg(format!("cannot read config '{config_path}': {e}")))?;
        Self::from_hjson_str(&raw)
    }
}

/// Process-wide atomic counters.  Surfaced via `v2/status.rebalancer`
/// and a future `v2/rebalancer.settings` RPC.  All loads are Relaxed
/// — these are dashboard numbers, not synchronisation primitives.
#[derive(Default)]
pub struct RebalancerStats {
    pub records_replicated_lifetime: AtomicU64,
    pub records_replicated_last_run: AtomicU64,
    pub records_examined_lifetime:   AtomicU64,
    pub records_examined_last_run:   AtomicU64,
    pub batches_examined_lifetime:   AtomicU64,
    pub batches_examined_last_run:   AtomicU64,
    pub paused_for_lag_lifetime:     AtomicU64,
    pub errors_lifetime:             AtomicU64,
    pub last_run_ts:                 AtomicU64,
    pub last_run_ms:                 AtomicU64,
}

static STATS: OnceLock<RebalancerStats> = OnceLock::new();
pub fn stats() -> &'static RebalancerStats {
    STATS.get_or_init(RebalancerStats::default)
}

/// One sweep's outcome.  Aggregated by the background task into the
/// process-wide [`RebalancerStats`] via [`record_run`].
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub records_examined:   u64,
    pub records_replicated: u64,
    pub batches_examined:   u64,
    pub paused_for_lag:     bool,
    pub errors:             u64,
    pub elapsed_ms:         u64,
}

/// Roll a [`ScanReport`] into the process-wide [`RebalancerStats`].
pub fn record_run(r: &ScanReport) {
    let s = stats();
    s.records_replicated_lifetime.fetch_add(r.records_replicated, Ordering::Relaxed);
    s.records_replicated_last_run.store(r.records_replicated, Ordering::Relaxed);
    s.records_examined_lifetime  .fetch_add(r.records_examined,   Ordering::Relaxed);
    s.records_examined_last_run  .store(r.records_examined,   Ordering::Relaxed);
    s.batches_examined_lifetime  .fetch_add(r.batches_examined,   Ordering::Relaxed);
    s.batches_examined_last_run  .store(r.batches_examined,   Ordering::Relaxed);
    s.errors_lifetime            .fetch_add(r.errors,             Ordering::Relaxed);
    s.last_run_ms                .store(r.elapsed_ms,             Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    s.last_run_ts.store(now, Ordering::Relaxed);
    if r.paused_for_lag {
        s.paused_for_lag_lifetime.fetch_add(1, Ordering::Relaxed);
    }
}

/// Pure helper: given a record and the set of peer IDs that already
/// hold a copy (probed via `v2/cluster.has_records`), return the peer
/// IDs that should receive a copy to reach `min_rf` total replicas.
///
/// `self_id` is the local node's ID; the local copy always counts
/// toward the replication target.  Returns empty when already at or
/// above the target.  Peers are picked in the input order — the
/// caller controls the deterministic ordering (typically peer ID
/// hash or membership order) so two nodes scanning the same record
/// agree on which peer to push to.
pub fn pick_peers_to_push(
    have:      &HashSet<Uuid>,
    all_peers: &[Uuid],
    self_id:   Uuid,
    min_rf:    usize,
) -> Vec<Uuid> {
    if min_rf == 0 {
        return Vec::new();
    }
    let mut current: HashSet<Uuid> = have.clone();
    current.insert(self_id); // local copy counts toward the target
    if current.len() >= min_rf {
        return Vec::new();
    }
    let needed = min_rf - current.len();
    let mut missing: Vec<Uuid> = all_peers
        .iter()
        .copied()
        .filter(|p| !current.contains(p) && *p != self_id)
        .collect();
    missing.truncate(needed);
    missing
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests — pure helpers, no I/O.
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(n: u8) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[15] = n;
        Uuid::from_bytes(bytes)
    }

    #[test]
    fn pick_returns_empty_when_target_met_by_self() {
        // rf=1, local always counts → no peers needed even if none have it.
        let have: HashSet<Uuid> = HashSet::new();
        let all = vec![uuid(2), uuid(3)];
        let got = pick_peers_to_push(&have, &all, uuid(1), 1);
        assert!(got.is_empty());
    }

    #[test]
    fn pick_returns_missing_peers_up_to_target() {
        // rf=3, local + 0 peers have it → need 2 more.  Two peers available.
        let have: HashSet<Uuid> = HashSet::new();
        let all = vec![uuid(2), uuid(3), uuid(4)];
        let got = pick_peers_to_push(&have, &all, uuid(1), 3);
        assert_eq!(got, vec![uuid(2), uuid(3)]);
    }

    #[test]
    fn pick_skips_peers_that_already_have_the_record() {
        // rf=3, local + peer 2 have it → need 1 more.
        let have: HashSet<Uuid> = [uuid(2)].into_iter().collect();
        let all = vec![uuid(2), uuid(3), uuid(4)];
        let got = pick_peers_to_push(&have, &all, uuid(1), 3);
        assert_eq!(got, vec![uuid(3)]);
    }

    #[test]
    fn pick_excludes_self_from_targets() {
        // self_id appears in all_peers (defensive); must not be picked.
        let have: HashSet<Uuid> = HashSet::new();
        let all = vec![uuid(1), uuid(2)];
        let got = pick_peers_to_push(&have, &all, uuid(1), 2);
        assert_eq!(got, vec![uuid(2)]);
    }

    #[test]
    fn pick_returns_empty_when_min_rf_is_zero() {
        let have: HashSet<Uuid> = HashSet::new();
        let all = vec![uuid(2), uuid(3)];
        assert!(pick_peers_to_push(&have, &all, uuid(1), 0).is_empty());
    }

    #[test]
    fn pick_truncates_when_more_peers_than_needed() {
        // rf=2, local has it → need 1; 3 peers available → pick first 1.
        let have: HashSet<Uuid> = HashSet::new();
        let all = vec![uuid(2), uuid(3), uuid(4)];
        let got = pick_peers_to_push(&have, &all, uuid(1), 2);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], uuid(2));
    }

    #[test]
    fn pick_handles_short_peer_list_gracefully() {
        // rf=5 but only 2 other peers in the cluster.  Push to all of them,
        // even though we can't reach the target — the cluster is just smaller
        // than the requested rf, which is operator error not our problem.
        let have: HashSet<Uuid> = HashSet::new();
        let all = vec![uuid(2), uuid(3)];
        let got = pick_peers_to_push(&have, &all, uuid(1), 5);
        assert_eq!(got, vec![uuid(2), uuid(3)]);
    }

    #[test]
    fn config_disabled_when_block_missing() {
        let cfg = RebalancerConfig::from_hjson_str(r#"{ "dbpath": "/tmp/x" }"#).unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn config_disabled_when_enabled_false() {
        let cfg = RebalancerConfig::from_hjson_str(
            r#"{ "rebalancer": { "enabled": false } }"#
        ).unwrap();
        assert!(!cfg.enabled);
    }

    #[test]
    fn config_parses_full_block() {
        let raw = r#"{
            "rebalancer": {
                "enabled":                      true,
                "interval":                     "30s",
                "batch_size":                   25,
                "max_per_run":                  100,
                "min_replication_factor":       2,
                "pause_if_ingest_lag_p95_ms":   500,
                "pause_if_ingest_flush_p95_ms": 3000
            }
        }"#;
        let cfg = RebalancerConfig::from_hjson_str(raw).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.interval, Duration::from_secs(30));
        assert_eq!(cfg.batch_size, 25);
        assert_eq!(cfg.max_per_run, 100);
        assert_eq!(cfg.min_replication_factor, Some(2));
        assert_eq!(cfg.pause_if_ingest_lag_p95_ms, 500);
        assert_eq!(cfg.pause_if_ingest_flush_p95_ms, 3000);
    }

    #[test]
    fn config_pause_flush_defaults_when_absent() {
        let cfg = RebalancerConfig::from_hjson_str(
            r#"{ "rebalancer": { "enabled": true } }"#
        ).unwrap();
        assert_eq!(cfg.pause_if_ingest_flush_p95_ms, DEFAULT_FLUSH_PAUSE_MS);
    }

    #[test]
    fn config_batch_size_clamps() {
        // Out-of-range values get clamped, not rejected.
        let cfg = RebalancerConfig::from_hjson_str(
            r#"{ "rebalancer": { "enabled": true, "batch_size": 0 } }"#
        ).unwrap();
        assert!(cfg.batch_size >= 1);
        let cfg = RebalancerConfig::from_hjson_str(
            r#"{ "rebalancer": { "enabled": true, "batch_size": 999999 } }"#
        ).unwrap();
        assert!(cfg.batch_size <= 1000);
    }
}
