//! Cron-driven scheduler for stored BUND scripts.
//!
//! The scheduler reads the script registry exposed by
//! [`ShardsManager::scripts`], parses each `schedule` field as a crontab
//! expression via [`croner::Cron`], and submits any script whose next
//! occurrence falls within the current minute to the persistent
//! [`crate::vm::workers::BundWorkerPool`].
//!
//! Each submission re-uses the script's storage UUID as the result-queue id
//! (via [`submit_script_with_id`]) so that callers can locate the latest
//! workbench output by querying the well-known queue.
//!
//! The scheduler itself is stateless — every tick rebuilds the in-memory
//! `(uuid → Cron)` map from the registry, so newly added scripts are picked
//! up immediately and deleted scripts disappear without restart.
//!
//! ## Cluster mode
//!
//! When the scheduler is built with [`Scheduler::with_cluster`], every
//! tick — before submitting a script — queries this node's local
//! `cluster::scheduler_log` plus every Alive peer's
//! `v2/scheduler.last_seen` in parallel.  If **any** node executed the
//! same `script_id` within the configured `dedup_window`, this tick
//! suppresses the fire.  Otherwise the tick records its execution
//! locally and submits the script as in standalone mode.
//!
//! This is **eventually-consistent dedup**: the small race window
//! between "this node's check" and "this node's record" can let two
//! nodes ticking at the exact same instant both fire.  The dedup
//! window is typically minutes wide, so this only matters for ticks
//! whose duty-cycle is itself sub-second-aligned across nodes.
//!
//! ## Tick cadence
//!
//! [`Scheduler::run`] is meant to be invoked once per minute; running it
//! more often will fire scripts with cron `* * * * *` more than once per
//! minute (because the same minute boundary is "current" for multiple ticks).
//! For sub-minute precision, switch to a smaller cadence and supply
//! second-level cron patterns (`croner` supports the optional 6-field form
//! with `with_seconds_required()` if needed).

use crate::cluster::{fanout, Cluster};
use crate::common::error::{err_msg, Result};
use crate::shardsmanager::ShardsManager;
use crate::vm::workers::submit_script_with_id;
use chrono::{Duration, Timelike, Utc};
use croner::Cron;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Cron-driven dispatcher of stored BUND scripts.
///
/// Holds a clone of the `ShardsManager` so each tick can read the script
/// registry and fetch script bodies without requiring the global singleton.
pub struct Scheduler {
    db: ShardsManager,
    /// `Some(...)` when this node should coordinate with peers before
    /// firing each script; `None` for standalone-mode scheduling.
    cluster: Option<Arc<Cluster>>,
    /// Suppress this node's fire when any peer (or this node) executed
    /// the same script within the trailing `dedup_window`.  Honoured
    /// only when `cluster` is `Some`.
    dedup_window_secs: u64,
}

impl Scheduler {
    /// Construct a standalone scheduler.  Identical to pre-cluster
    /// behaviour: every tick fires every script whose cron resolves to
    /// the current minute, independently of any peers.
    pub fn new(db: ShardsManager) -> Self {
        Self { db, cluster: None, dedup_window_secs: 0 }
    }

    /// Construct a cluster-aware scheduler.  `dedup_window_secs` is the
    /// trailing window over which an execution by ANY node suppresses
    /// this node's fire of the same script.  Typically read from
    /// `cluster.scheduler_dedup_window` in `bds.hjson`.
    pub fn with_cluster(db: ShardsManager, cluster: Arc<Cluster>, dedup_window_secs: u64) -> Self {
        Self { db, cluster: Some(cluster), dedup_window_secs }
    }

    /// Single tick: enumerate stored scripts, fire those whose cron pattern
    /// resolves to a moment within the current minute, and return how many
    /// scripts were dispatched this tick.
    ///
    /// Errors fetching individual scripts or parsing individual cron strings
    /// are logged and skipped — one bad entry never aborts the whole tick.
    pub fn run(&self) -> Result<usize> {
        // Cron schedules are evaluated against UTC wall-clock, consistent
        // with shard boundaries, telemetry timestamps, and every other
        // time surface in the system.  A pattern like `0 9 * * *` fires
        // at 09:00 UTC regardless of the host's local timezone.
        let now = Utc::now();
        let minute_start = now
            .with_nanosecond(0)
            .and_then(|t| t.with_second(0))
            .ok_or_else(|| err_msg("scheduler: minute truncation failed"))?;
        let minute_end = minute_start + Duration::minutes(1);

        // Snapshot the registry: (uuid, schedule_string) pairs.
        let entries = self
            .db
            .scripts()
            .map_err(|e| err_msg(format!("scheduler: scripts() failed: {e}")))?;

        // Build the ephemeral (uuid → Cron) map. Invalid patterns are logged
        // and dropped rather than failing the whole tick.
        let mut crons: HashMap<Uuid, Cron> = HashMap::with_capacity(entries.len());
        for (id, schedule) in &entries {
            match Cron::new(schedule).parse() {
                Ok(cron) => {
                    crons.insert(*id, cron);
                }
                Err(e) => log::warn!(
                    "[scheduler] invalid cron schedule {schedule:?} for script {id}: {e}"
                ),
            }
        }

        // Best-effort housekeeping: prune log rows older than 2× the
        // dedup window so the file stays bounded.  Cluster-mode only.
        if let Some(cluster) = &self.cluster {
            let prune_cutoff = self.dedup_window_secs.saturating_mul(2).max(60);
            if let Err(e) = cluster.scheduler_log.prune_older_than(prune_cutoff) {
                log::warn!("[scheduler] scheduler_log prune failed: {e}");
            }
        }

        let mut fired = 0usize;
        for (id, cron) in &crons {
            // `find_next_occurrence` with `inclusive=true` returns the first
            // occurrence at or after `minute_start`. If that moment falls
            // before `minute_end`, the cron pattern fires this minute.
            let next = match cron.find_next_occurrence(&minute_start, true) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!(
                        "[scheduler] find_next_occurrence failed for script {id}: {e}"
                    );
                    continue;
                }
            };
            if next < minute_end {
                // Cluster-mode dedup: skip if any node ran this script
                // within the dedup window.  Standalone mode falls
                // straight through to the local fire.
                if let Some(cluster) = &self.cluster {
                    match self.recently_executed_anywhere(cluster, *id) {
                        Ok(true) => {
                            log::debug!(
                                "[scheduler] skip {id} — already executed cluster-wide \
                                 within {}s",
                                self.dedup_window_secs
                            );
                            continue;
                        }
                        Ok(false) => {} // proceed to fire
                        Err(e) => {
                            // Failing the dedup query is a cluster fault;
                            // erring on the side of "fire" here means a
                            // network blip can produce duplicate runs.
                            // Erring on the side of "skip" means a
                            // network blip silently drops jobs.  We pick
                            // "fire" (current historic behaviour) and
                            // log loudly.
                            log::warn!(
                                "[scheduler] dedup check for {id} failed ({e}); firing anyway"
                            );
                        }
                    }
                    if let Err(e) = cluster.scheduler_log.record(*id, cluster.node_id, now_secs()) {
                        log::warn!(
                            "[scheduler] failed to record execution of {id} in scheduler_log: {e}"
                        );
                    }
                }

                let body = match self.db.script(*id) {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        log::warn!(
                            "[scheduler] script {id} disappeared between scripts() and script(); skipping"
                        );
                        continue;
                    }
                    Err(e) => {
                        log::warn!("[scheduler] script({id}) lookup failed: {e}");
                        continue;
                    }
                };

                match submit_script_with_id(*id, &body) {
                    Ok(_) => {
                        log::info!(
                            "[scheduler] submitted script {id} (cron tick at {next})"
                        );
                        fired += 1;
                    }
                    Err(e) => {
                        log::warn!(
                            "[scheduler] submit_script_with_id({id}) failed: {e}"
                        );
                    }
                }
            }
        }

        Ok(fired)
    }

    /// Has any node — local or peer — executed `script_id` within the
    /// trailing `dedup_window_secs`?  Queries this node's local log
    /// plus fans `v2/scheduler.last_seen` out to every Alive peer in
    /// parallel.  Returns the boolean answer based on the **max**
    /// observed `last_executed_at`.
    ///
    /// The fan-out is driven through tokio via `runtime::block_on` so
    /// this method stays sync-callable from the existing
    /// `spawn_blocking` Scheduler tick.
    fn recently_executed_anywhere(
        &self,
        cluster: &Arc<Cluster>,
        script_id: Uuid,
    ) -> Result<bool> {
        let now = now_secs();
        let cutoff = now.saturating_sub(self.dedup_window_secs);

        // Local log first — cheap, in-process, and short-circuits the
        // expensive fan-out when this node already serviced the tick
        // (the common case in steady state with sub-minute dedup
        // windows).
        if let Some(ts) = cluster.scheduler_log.last_executed(script_id)? {
            if ts >= cutoff {
                return Ok(true);
            }
        }

        // Fan-out only if there's at least one Alive peer to ask.
        let alive = cluster.peers.read().alive_count();
        if alive == 0 {
            return Ok(false);
        }

        let cluster_clone = cluster.clone();
        let params = serde_json::json!({ "script_id": script_id.to_string() });
        let results = crate::vm::api::runtime::block_on(async move {
            fanout::fan_out_v2(&cluster_clone, "v2/scheduler.last_seen", params).await
        });

        for r in results.ok_results() {
            if let Some(ts) = r.get("last_executed_at").and_then(|v| v.as_u64()) {
                if ts >= cutoff {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
