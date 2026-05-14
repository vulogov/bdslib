//! Background tokio task — the shard rebuild healer (Phase 2 self-healing).
//!
//! Quarantine (`shardhealth` + the `quarantined` catalog column) is the
//! *detection + isolation* half: a shard whose storage keeps failing to
//! open is flagged and kept out of the read/write path so the rest of
//! the node keeps serving.  This task is the *recovery* half — every
//! `interval` it walks `ShardsManager::list_quarantined_shards()` and
//! attempts a repair via `rebuild_quarantined_shard`:
//!
//! 1. **Transient retry** — re-open the shard; clears the quarantine if
//!    it opens cleanly (the quarantine was a false positive).
//! 2. **Index rebuild** — if DuckDB is intact but the Tantivy / HNSW
//!    directories are corrupt, delete + rebuild them from DuckDB.
//! 3. **Unhealable** — DuckDB itself is corrupt; the shard stays
//!    quarantined and is reported `Failed` in the health registry for
//!    operator (or future peer-rebuild) intervention.
//!
//! Config (`self_healing:` block in `bds.hjson`):
//!
//! | key                          | default | description                                 |
//! |------------------------------|---------|---------------------------------------------|
//! | `enabled`                    | `true`  | Master switch.  On by default — the healer  |
//! |                              |         | only acts on *already-quarantined* shards.  |
//! | `interval`                   | `"60s"` | Humantime cadence between heal sweeps.      |
//! | `recreate_failed_shards`     | `false` | Tier-3 escalation switch (see below).  Off  |
//! |                              |         | by default — it is **destructive**.         |
//! | `failed_shard_recreate_after`| `"1h"`  | How long a shard must stay unhealable before|
//! |                              |         | Tier-3 may recreate it.                     |
//!
//! ## Tier-3 — recreate failed shards
//!
//! When a shard is *unhealable* (DuckDB itself corrupt — no local
//! rebuild possible) it normally stays quarantined and `FAILED`
//! forever, waiting for an operator.  Tier-3 automates the recovery
//! **when, and only when, it is safe**: if a shard has been unhealable
//! for longer than `failed_shard_recreate_after`, AND
//! `recreate_failed_shards` is `true`, AND the cluster rebalancer is
//! enabled, AND the node is actually in cluster mode, the healer
//! **destroys** the failed shard and recreates it empty for the same
//! interval.  Peers' rebalancers then repopulate it via
//! `v2/cluster.replicate_record`.
//!
//! If `recreate_failed_shards` is off, or the rebalancer is disabled,
//! or the node is standalone, the shard simply stays `FAILED` — the
//! safe default.  The cluster-mode check is a hard safety net: a
//! standalone node has no peers to repopulate from, so recreating
//! there would be permanent data loss regardless of the flags.
//!
//! Disabled-handle semantics, oneshot shutdown, `spawn_blocking` for
//! the DB-heavy rebuild, `supervise::tick` panic isolation, and a
//! `health` heartbeat all mirror the other background tasks.

use crate::server::supervise;
use bdslib::shardscache::RebuildOutcome;
use std::time::Duration;
use tokio::sync::oneshot;

const DEFAULT_INTERVAL_SECS:           u64 = 60;
const MIN_INTERVAL_SECS:               u64 = 10;
const MAX_INTERVAL_SECS:               u64 = 3_600;
const DEFAULT_RECREATE_AFTER_SECS:     u64 = 3_600;  // 1h
const DEFAULT_CONSISTENCY_INTERVAL_SECS: u64 = 600;  // 10m

/// Parsed `self_healing:` block, plus the cross-referenced
/// `rebalancer.enabled` flag (Tier-3 only runs when the rebalancer is
/// on, so the healer needs to know).
pub struct Config {
    pub enabled:                       bool,
    pub interval_secs:                 u64,
    /// Tier-3 master switch.  Default `false` — destructive.
    pub recreate_failed_shards:        bool,
    /// How long a shard must be continuously unhealable before Tier-3
    /// may recreate it.
    pub failed_shard_recreate_after_secs: u64,
    /// Cadence of the Phase-3 cross-engine consistency sweep, in
    /// seconds.  `0` disables the sweep.  Independent of, and
    /// typically much slower than, `interval_secs` — the consistency
    /// sweep opens every sealed shard, so it is deliberately
    /// infrequent.
    pub consistency_interval_secs:     u64,
    /// Mirror of `rebalancer.enabled` — Tier-3 is a no-op without it
    /// (a recreated empty shard would never be repopulated).  Passed
    /// in by `main` from the already-parsed `RebalancerConfig`.
    pub rebalancer_enabled:            bool,
}

impl Config {
    fn default_with(rebalancer_enabled: bool) -> Self {
        Self {
            enabled:                          true,
            interval_secs:                    DEFAULT_INTERVAL_SECS,
            recreate_failed_shards:           false,
            failed_shard_recreate_after_secs: DEFAULT_RECREATE_AFTER_SECS,
            consistency_interval_secs:        DEFAULT_CONSISTENCY_INTERVAL_SECS,
            rebalancer_enabled,
        }
    }

    /// Read `self_healing.*` from the hjson config.  A missing block
    /// keeps the defaults (enabled, 60 s sweep, Tier-3 off) — the
    /// healer is on by default because it only ever touches
    /// already-quarantined shards, so the risk of leaving it on is
    /// minimal and the cost of leaving it off is "a corrupt shard
    /// never recovers".  `rebalancer_enabled` is supplied by the
    /// caller from the already-parsed `RebalancerConfig`.
    pub fn from_config(config_path: Option<&str>, rebalancer_enabled: bool) -> Self {
        let path = match config_path {
            Some(p) => p.to_string(),
            None => match std::env::var("BDS_CONFIG") {
                Ok(p) => p,
                Err(_) => return Self::default_with(rebalancer_enabled),
            },
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(_) => return Self::default_with(rebalancer_enabled),
        };
        let val: serde_hjson::Value = match serde_hjson::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return Self::default_with(rebalancer_enabled),
        };
        let block = val.as_object()
            .and_then(|o| o.get("self_healing"))
            .and_then(|v| v.as_object());
        let block = match block {
            Some(b) => b,
            None    => return Self::default_with(rebalancer_enabled),
        };

        let enabled = block.get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let interval_secs = match block.get("interval").and_then(|v| v.as_str()) {
            Some(s) => humantime::parse_duration(s)
                .map(|d| d.as_secs())
                .unwrap_or(DEFAULT_INTERVAL_SECS),
            None => DEFAULT_INTERVAL_SECS,
        }.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);

        let recreate_failed_shards = block.get("recreate_failed_shards")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let failed_shard_recreate_after_secs =
            match block.get("failed_shard_recreate_after").and_then(|v| v.as_str()) {
                Some(s) => humantime::parse_duration(s)
                    .map(|d| d.as_secs())
                    .unwrap_or(DEFAULT_RECREATE_AFTER_SECS),
                None => DEFAULT_RECREATE_AFTER_SECS,
            };

        // `consistency_interval` — humantime; an explicit "0s" (or any
        // value that parses to zero) disables the consistency sweep.
        let consistency_interval_secs =
            match block.get("consistency_interval").and_then(|v| v.as_str()) {
                Some(s) => humantime::parse_duration(s)
                    .map(|d| d.as_secs())
                    .unwrap_or(DEFAULT_CONSISTENCY_INTERVAL_SECS),
                None => DEFAULT_CONSISTENCY_INTERVAL_SECS,
            };

        Self {
            enabled,
            interval_secs,
            recreate_failed_shards,
            failed_shard_recreate_after_secs,
            consistency_interval_secs,
            rebalancer_enabled,
        }
    }
}

/// Handle returned by [`start`].  Idempotent on disabled handles.
pub struct Handle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    task:        Option<tokio::task::JoinHandle<()>>,
}

impl Handle {
    fn disabled() -> Self {
        Self { shutdown_tx: None, task: None }
    }

    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            if let Err(e) = task.await {
                log::error!("[shard-healer] task panicked on shutdown: {e:?}");
            }
        }
    }
}

/// Effective Tier-3 recreation policy for one healer run.  `Some`
/// only when every precondition holds; `None` means "shards that
/// can't be rebuilt stay FAILED" (the safe default).
#[derive(Clone, Copy)]
struct RecreatePolicy {
    /// Minimum continuous-unhealable age before a shard may be
    /// recreated.
    after_secs: u64,
}

/// Spawn the shard rebuild healer.  Returns a no-op handle when
/// `self_healing.enabled = false`.
pub fn start(cfg: Config) -> Handle {
    if !cfg.enabled {
        log::info!("[shard-healer] disabled (self_healing.enabled=false)");
        return Handle::disabled();
    }
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let interval_secs = cfg.interval_secs;

    // Resolve the Tier-3 policy once at startup.  It is `Some` only
    // when the operator opted in AND the rebalancer is enabled — the
    // per-sweep cluster-mode check is the final gate inside the sweep.
    let recreate = if cfg.recreate_failed_shards && cfg.rebalancer_enabled {
        Some(RecreatePolicy { after_secs: cfg.failed_shard_recreate_after_secs })
    } else {
        None
    };

    let consistency_secs = cfg.consistency_interval_secs;
    let task = tokio::spawn(run(interval_secs, consistency_secs, recreate, shutdown_rx));
    let consistency_desc = if consistency_secs == 0 {
        "consistency sweep DISABLED".to_string()
    } else {
        format!("consistency sweep every {consistency_secs}s")
    };
    match &recreate {
        Some(p) => log::info!(
            "[shard-healer] started — heal sweep every {interval_secs}s; {consistency_desc}; \
             Tier-3 recreate ENABLED (after {}s unhealable)",
            p.after_secs,
        ),
        None => {
            let why = if !cfg.recreate_failed_shards {
                "recreate_failed_shards=false"
            } else {
                "rebalancer disabled"
            };
            log::info!(
                "[shard-healer] started — heal sweep every {interval_secs}s; {consistency_desc}; \
                 Tier-3 recreate DISABLED ({why}) — unhealable shards stay FAILED",
            );
        }
    }
    Handle {
        shutdown_tx: Some(shutdown_tx),
        task:        Some(task),
    }
}

async fn run(
    interval_secs:     u64,
    consistency_secs:  u64,
    recreate:          Option<RecreatePolicy>,
    mut shutdown_rx:   oneshot::Receiver<()>,
) {
    let interval = Duration::from_secs(interval_secs);
    // Independent, slower cadence for the consistency sweep — tracked
    // by elapsed time on the heal-loop wakeup rather than a second
    // `select!` timer (two `sleep` arms would reset each other).
    let consistency_interval = Duration::from_secs(consistency_secs);
    let mut last_consistency = std::time::Instant::now();
    // Health source — stale window 4× the sweep interval.
    bdslib::health::register(
        "shard_healer",
        interval_secs.saturating_mul(4).max(120),
    );
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                log::debug!("[shard-healer] shutdown signal received — stopping");
                break;
            }
            _ = tokio::time::sleep(interval) => {
                bdslib::health::heartbeat("shard_healer");
                // Panic-isolate the sweep — a panic in a rebuild must
                // not kill the healer loop.
                supervise::tick("shard_healer", heal_sweep(recreate)).await;

                // Consistency sweep on its own (slower) cadence —
                // piggybacks on this wakeup but only actually runs
                // every `consistency_interval`.  Disabled when
                // `consistency_secs == 0`.
                if consistency_secs > 0
                    && last_consistency.elapsed() >= consistency_interval
                {
                    last_consistency = std::time::Instant::now();
                    supervise::tick("shard_healer", consistency_sweep()).await;
                }
            }
        }
    }
}

/// One cross-engine consistency sweep — delegates to
/// `ShardsManager::consistency_sweep`, which walks every sealed shard,
/// compares DuckDB / Tantivy / HNSW counts, and quarantines any that
/// have drifted (the rebuild healer then re-indexes them on its next
/// pass).  Runs on `spawn_blocking` because it opens shards and issues
/// DuckDB count queries.
async fn consistency_sweep() {
    let result = tokio::task::spawn_blocking(|| {
        let db = bdslib::get_db().map_err(|e| format!("get_db: {e}"))?;
        db.consistency_sweep().map_err(|e| format!("consistency_sweep: {e}"))
    }).await;

    match result {
        Ok(Ok(report)) => {
            if report.divergent > 0 || report.open_failures > 0 {
                log::warn!(
                    "[shard-healer] consistency sweep: checked={} divergent={} \
                     open_failures={} — divergent shards quarantined for rebuild",
                    report.checked, report.divergent, report.open_failures,
                );
            } else if report.checked > 0 {
                log::debug!(
                    "[shard-healer] consistency sweep: {} sealed shard(s) all consistent",
                    report.checked,
                );
            }
        }
        Ok(Err(e)) => log::warn!("[shard-healer] consistency sweep failed: {e}"),
        Err(join_err) => log::error!(
            "[shard-healer] consistency sweep task panicked: {join_err}"
        ),
    }
}

/// One heal sweep — walk every quarantined shard and attempt a repair.
/// Each shard's rebuild runs on `spawn_blocking` (DuckDB reads + fs
/// deletes + index replay are synchronous and can take a moment).
///
/// `recreate` carries the resolved Tier-3 policy: when `Some`, an
/// *unhealable* shard that has been failing longer than
/// `after_secs` — and only on a node that is actually in cluster
/// mode — is destroyed and recreated empty for the rebalancer to
/// repopulate.  When `None`, unhealable shards stay `FAILED`.
async fn heal_sweep(recreate: Option<RecreatePolicy>) {
    let quarantined = {
        let db = match bdslib::get_db() {
            Ok(db) => db,
            Err(e) => {
                log::warn!("[shard-healer] get_db failed, skipping sweep: {e}");
                return;
            }
        };
        match db.list_quarantined_shards() {
            Ok(v)  => v,
            Err(e) => {
                log::warn!("[shard-healer] list_quarantined_shards failed: {e}");
                return;
            }
        }
    };

    if quarantined.is_empty() {
        return; // healthy — nothing to do, no log noise
    }
    log::info!(
        "[shard-healer] {} shard(s) quarantined — attempting repair",
        quarantined.len()
    );

    for info in quarantined {
        let shard_id   = info.shard_id;
        let health_name = format!(
            "shard.{}_{}",
            bdslib::shardhealth::key_of(info.start_time, info.end_time).0,
            bdslib::shardhealth::key_of(info.start_time, info.end_time).1,
        );

        let info_for_blocking = info.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| format!("get_db: {e}"))?;
            db.rebuild_quarantined_shard(&info_for_blocking)
                .map_err(|e| format!("rebuild: {e}"))
        }).await;

        match outcome {
            Ok(Ok(RebuildOutcome::Transient)) => {
                bdslib::shardhealth::tracker().record_heal();
                bdslib::health::report(&health_name, bdslib::health::HealthStatus::Healthy);
                log::info!(
                    "[shard-healer] shard {shard_id} recovered (transient — re-opened cleanly)"
                );
            }
            Ok(Ok(RebuildOutcome::Reindexed { reindexed })) => {
                bdslib::shardhealth::tracker().record_heal();
                bdslib::health::report(&health_name, bdslib::health::HealthStatus::Healthy);
                log::info!(
                    "[shard-healer] shard {shard_id} rebuilt — {reindexed} primary \
                     record(s) re-indexed into fresh FTS + vector indexes"
                );
            }
            Ok(Ok(RebuildOutcome::Unhealable { reason })) => {
                // Stamp the first-unhealable time (idempotent across
                // sweeps) so we can measure how long this shard has
                // been FAILED.
                let key = bdslib::shardhealth::key_of(
                    info.start_time, info.end_time);
                let first_failed_ts =
                    bdslib::shardhealth::tracker().mark_unhealable(key);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let failed_for = now.saturating_sub(first_failed_ts);

                // ── Tier-3 escalation: recreate the failed shard? ──
                // Gated on: operator opted in + rebalancer enabled
                // (both folded into `recreate`), the shard has been
                // FAILED long enough, AND — the hard safety net — the
                // node is actually in cluster mode.  A standalone node
                // has no peers to repopulate from; recreating there
                // would be permanent data loss, so we never do it.
                let may_recreate = recreate
                    .map(|p| failed_for >= p.after_secs)
                    .unwrap_or(false);
                let in_cluster = bdslib::get_db()
                    .ok()
                    .map(|db| db.cluster().is_some())
                    .unwrap_or(false);

                if may_recreate && in_cluster {
                    let info_for_recreate = info.clone();
                    let recreate_result = tokio::task::spawn_blocking(move || {
                        let db = bdslib::get_db()
                            .map_err(|e| format!("get_db: {e}"))?;
                        db.recreate_failed_shard(&info_for_recreate)
                            .map_err(|e| format!("recreate: {e}"))
                    }).await;
                    match recreate_result {
                        Ok(Ok(())) => {
                            bdslib::shardhealth::tracker().record_recreation();
                            bdslib::health::report(
                                &health_name,
                                bdslib::health::HealthStatus::Healthy,
                            );
                            log::warn!(
                                "[shard-healer] shard {shard_id} was unhealable for \
                                 {failed_for}s — RECREATED empty for the same interval; \
                                 the rebalancer will repopulate it from peers"
                            );
                        }
                        Ok(Err(e)) => {
                            // Recreate itself failed — leave it FAILED;
                            // the next sweep retries.
                            log::error!(
                                "[shard-healer] shard {shard_id} recreate FAILED: {e} \
                                 — stays quarantined"
                            );
                            bdslib::health::report(
                                &health_name,
                                bdslib::health::HealthStatus::Failed(format!(
                                    "unhealable + recreate failed: {e}"
                                )),
                            );
                        }
                        Err(join_err) => {
                            log::error!(
                                "[shard-healer] shard {shard_id} recreate task \
                                 panicked: {join_err}"
                            );
                        }
                    }
                } else {
                    // Stays FAILED — either Tier-3 is off / rebalancer
                    // disabled / standalone, or the shard hasn't been
                    // FAILED long enough yet.
                    let detail = if recreate.is_none() {
                        "Tier-3 recreate disabled".to_string()
                    } else if !in_cluster {
                        "node is standalone — no peers to repopulate from".to_string()
                    } else {
                        format!(
                            "unhealable for {failed_for}s, recreate window is {}s",
                            recreate.map(|p| p.after_secs).unwrap_or(0),
                        )
                    };
                    bdslib::health::report(
                        &health_name,
                        bdslib::health::HealthStatus::Failed(format!(
                            "unhealable from local data: {reason}"
                        )),
                    );
                    log::error!(
                        "[shard-healer] shard {shard_id} CANNOT self-heal — \
                         stays FAILED ({detail}): {reason}"
                    );
                }
            }
            Ok(Err(e)) => {
                // Rebuild attempt itself errored (e.g. fs delete
                // failed) — the shard stays quarantined for the next
                // sweep to retry.
                log::warn!("[shard-healer] shard {shard_id} rebuild attempt failed: {e}");
            }
            Err(join_err) => {
                log::error!(
                    "[shard-healer] shard {shard_id} rebuild task panicked: {join_err}"
                );
            }
        }
    }
}
