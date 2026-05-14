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
//! | key        | default  | description                                  |
//! |------------|----------|----------------------------------------------|
//! | `enabled`  | `true`   | Master switch.  On by default — the healer   |
//! |            |          | only acts on *already-quarantined* shards,   |
//! |            |          | so the blast radius is small.                |
//! | `interval` | `"60s"`  | Humantime cadence between heal sweeps.       |
//!
//! Disabled-handle semantics, oneshot shutdown, `spawn_blocking` for
//! the DB-heavy rebuild, `supervise::tick` panic isolation, and a
//! `health` heartbeat all mirror the other background tasks.

use crate::server::supervise;
use bdslib::shardscache::RebuildOutcome;
use std::time::Duration;
use tokio::sync::oneshot;

const DEFAULT_INTERVAL_SECS: u64 = 60;
const MIN_INTERVAL_SECS:     u64 = 10;
const MAX_INTERVAL_SECS:     u64 = 3_600;

/// Parsed `self_healing:` block.
pub struct Config {
    pub enabled:       bool,
    pub interval_secs: u64,
}

impl Config {
    fn default() -> Self {
        Self { enabled: true, interval_secs: DEFAULT_INTERVAL_SECS }
    }

    /// Read `self_healing.*` from the hjson config.  A missing block
    /// keeps the defaults (enabled, 60 s) — the healer is on by
    /// default because it only ever touches already-quarantined
    /// shards, so the risk of leaving it on is minimal and the cost
    /// of leaving it off is "a corrupt shard never recovers".
    pub fn from_config(config_path: Option<&str>) -> Self {
        let path = match config_path {
            Some(p) => p.to_string(),
            None => match std::env::var("BDS_CONFIG") {
                Ok(p) => p,
                Err(_) => return Self::default(),
            },
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(_) => return Self::default(),
        };
        let val: serde_hjson::Value = match serde_hjson::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return Self::default(),
        };
        let block = val.as_object()
            .and_then(|o| o.get("self_healing"))
            .and_then(|v| v.as_object());
        let block = match block {
            Some(b) => b,
            None    => return Self::default(),
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

        Self { enabled, interval_secs }
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

/// Spawn the shard rebuild healer.  Returns a no-op handle when
/// `self_healing.enabled = false`.
pub fn start(cfg: Config) -> Handle {
    if !cfg.enabled {
        log::info!("[shard-healer] disabled (self_healing.enabled=false)");
        return Handle::disabled();
    }
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let interval_secs = cfg.interval_secs;
    let task = tokio::spawn(run(interval_secs, shutdown_rx));
    log::info!("[shard-healer] started — heal sweep every {interval_secs}s");
    Handle {
        shutdown_tx: Some(shutdown_tx),
        task:        Some(task),
    }
}

async fn run(interval_secs: u64, mut shutdown_rx: oneshot::Receiver<()>) {
    let interval = Duration::from_secs(interval_secs);
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
                supervise::tick("shard_healer", heal_sweep()).await;
            }
        }
    }
}

/// One heal sweep — walk every quarantined shard and attempt a repair.
/// Each shard's rebuild runs on `spawn_blocking` (DuckDB reads + fs
/// deletes + index replay are synchronous and can take a moment).
async fn heal_sweep() {
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
                bdslib::shardhealth::tracker().record_unhealable();
                bdslib::health::report(
                    &health_name,
                    bdslib::health::HealthStatus::Failed(format!(
                        "unhealable from local data: {reason}"
                    )),
                );
                log::error!(
                    "[shard-healer] shard {shard_id} CANNOT self-heal — stays quarantined: {reason}"
                );
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
