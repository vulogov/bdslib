//! Background tokio task that periodically calls
//! [`bdslib::retention::evict_expired`].
//!
//! Reads the `retention.*` block of `bds.hjson`, sleeps
//! `retention.interval_secs` between ticks, and runs each sweep on a
//! `tokio::task::spawn_blocking` (catalog reads + filesystem deletes
//! are synchronous and may take a moment on large shards).
//!
//! After every successful sweep:
//! - [`bdslib::retention::record_run`] copies the report into the
//!   process-wide stats record so `v2/status` can read it.
//! - When the sweep evicted any shard whose window overlaps
//!   `drain_load_duration`, the in-memory drain parser is reloaded so
//!   its cluster IDs no longer back-reference deleted templates.
//!
//! Config keys (under `retention:`):
//!
//! | hjson key                      | default | description                                                     |
//! |--------------------------------|---------|-----------------------------------------------------------------|
//! | `enabled`                      | `false` | Master switch.  Defaults off — operators opt in explicitly.    |
//! | `duration`                     | `30days`| Humantime retention window.                                     |
//! | `interval_secs`                | `300`   | Tick cadence.  Clamped to [60, 86400].  `0` disables the task. |
//! | `max_evictions_per_run`        | `50`    | Cap per tick.  `0` = no cap.                                   |
//! | `dry_run`                      | `false` | Log evictions but don't act.                                    |
//! | `reload_drain_after_evict`     | `true`  | Re-seed the drain parser when an evicted shard's window         |
//! |                                |         | overlaps `drain_load_duration`.                                |

use bdslib::retention::{evict_expired_with_quorum, record_run, RetentionConfig};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::oneshot;

/// Process-wide snapshot of the parsed retention config.  Populated
/// by [`start`] (or [`install_config`] when the background task is
/// disabled) so the JSON-RPC handlers see the live runtime settings
/// instead of re-reading `bds.hjson` from `$BDS_CONFIG`, which may
/// not match the file bdsnode was launched with via `--config`.
static ACTIVE: OnceLock<ActiveConfig> = OnceLock::new();

/// Slim snapshot suitable for JSON-RPC reflection.  Mirrors [`Config`]
/// but with `Send + Sync + 'static` baked in (only `Duration` is
/// borrowed-free; everything else is `String` / `bool` / `usize`).
#[derive(Debug, Clone)]
pub struct ActiveConfig {
    pub enabled:                  bool,
    pub duration:                 Duration,
    pub interval_secs:            u64,
    pub max_evictions_per_run:    usize,
    pub dry_run:                  bool,
    pub reload_drain_after_evict: bool,
    pub drain_load_duration:      Option<String>,
    /// Phase 3 — cluster-aware quorum.  See [`Config`] for prose.
    pub quorum_check_enabled:     bool,
    pub quorum_min_peers:         usize,
}

impl ActiveConfig {
    fn from_config(cfg: &Config) -> Self {
        Self {
            enabled:                  cfg.enabled,
            duration:                 cfg.duration,
            interval_secs:            cfg.interval_secs,
            max_evictions_per_run:    cfg.max_evictions_per_run,
            dry_run:                  cfg.dry_run,
            reload_drain_after_evict: cfg.reload_drain_after_evict,
            drain_load_duration:      cfg.drain_load_duration.clone(),
            quorum_check_enabled:     cfg.quorum_check_enabled,
            quorum_min_peers:         cfg.quorum_min_peers,
        }
    }
}

/// Borrow the currently-installed active config.  Returns `None` when
/// retention has never been initialised in this process (library
/// users that never call [`start`] — e.g. unit tests).
pub fn active() -> Option<&'static ActiveConfig> {
    ACTIVE.get()
}

/// Install the active config without spawning the background task.
/// Called by [`start`] internally; exposed because tests may want to
/// publish a config + drive `v2/retention.sweep` manually.
pub fn install_config(cfg: &Config) {
    let _ = ACTIVE.set(ActiveConfig::from_config(cfg));
}

const DEFAULT_INTERVAL_SECS:    u64   = 300;
const DEFAULT_DURATION_STR:     &str  = "30days";
const DEFAULT_MAX_PER_RUN:      usize = 50;
const MIN_INTERVAL_SECS:        u64   = 60;
const MAX_INTERVAL_SECS:        u64   = 86_400;

/// Parsed `retention.*` block from `bds.hjson`.
pub struct Config {
    pub enabled:                bool,
    pub duration:               Duration,
    pub interval_secs:          u64,
    pub max_evictions_per_run:  usize,
    pub dry_run:                bool,
    pub reload_drain_after_evict: bool,
    /// Snapshot of `drain_load_duration` so the task can decide whether
    /// to reload the drain parser without re-reading hjson on every
    /// tick.  When `None` drain is either disabled or unconfigured —
    /// the reload step is skipped regardless of
    /// `reload_drain_after_evict`.
    pub drain_load_duration:    Option<String>,

    // ── Phase 3 — cluster-aware quorum ────────────────────────────
    /// `retention.quorum_check_enabled` from `bds.hjson`.  Default
    /// **false** — Phase 3 is strictly opt-in.  When true, the
    /// sweeper pre-fetches every Alive peer's shard list and skips
    /// candidates that don't have `quorum_min_peers` replicas.
    pub quorum_check_enabled:   bool,
    /// `retention.quorum_min_peers` from `bds.hjson`.  Default 1.
    pub quorum_min_peers:       usize,
}

impl Config {
    pub fn from_config(config_path: Option<&str>) -> anyhow::Result<Self> {
        let path = match config_path {
            Some(p) => p.to_string(),
            None => match std::env::var("BDS_CONFIG") {
                Ok(p) => p,
                Err(_) => return Ok(Self::default()),
            },
        };

        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("cannot read config {path:?}: {e}"))?;
        let val: serde_hjson::Value = serde_hjson::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("hjson parse error in {path:?}: {e}"))?;
        let obj = val.as_object()
            .ok_or_else(|| anyhow::anyhow!("config must be a JSON object"))?;

        // Snapshot drain_load_duration before drilling into retention —
        // it lives at the top level alongside dbpath / shard_duration.
        let drain_load_duration = obj.get("drain_load_duration")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        let retention_obj = match obj.get("retention").and_then(|v| v.as_object()) {
            Some(o) => o,
            None    => {
                // No retention block at all → default (disabled).
                return Ok(Self {
                    drain_load_duration,
                    ..Self::default()
                });
            }
        };

        let enabled = retention_obj.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);

        let duration_str = retention_obj.get("duration")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_DURATION_STR);
        let duration = humantime::parse_duration(duration_str)
            .map_err(|e| anyhow::anyhow!(
                "retention.duration {duration_str:?}: {e}"
            ))?;
        if duration.is_zero() {
            anyhow::bail!("retention.duration must be > 0");
        }

        let interval_secs = retention_obj.get("interval_secs")
            .and_then(|v| v.as_f64())
            .map(|n| n as u64)
            .unwrap_or(DEFAULT_INTERVAL_SECS);
        // 0 disables the task without flipping `enabled` — useful for
        // operators who want to drive sweeps manually via the RPC.
        let interval_secs = if interval_secs == 0 {
            0
        } else {
            interval_secs.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS)
        };

        let max_evictions_per_run = retention_obj.get("max_evictions_per_run")
            .and_then(|v| v.as_f64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_PER_RUN);

        let dry_run = retention_obj.get("dry_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let reload_drain_after_evict = retention_obj.get("reload_drain_after_evict")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Phase 3 — cluster quorum.  Disabled by default per the user
        // spec; safety net for clusters with replication_factor ≥ 2.
        let quorum_check_enabled = retention_obj.get("quorum_check_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let quorum_min_peers = retention_obj.get("quorum_min_peers")
            .and_then(|v| v.as_f64())
            .map(|n| (n as usize).max(1))
            .unwrap_or(1);

        Ok(Self {
            enabled,
            duration,
            interval_secs,
            max_evictions_per_run,
            dry_run,
            reload_drain_after_evict,
            drain_load_duration,
            quorum_check_enabled,
            quorum_min_peers,
        })
    }

    fn default() -> Self {
        Self {
            enabled:               false,
            duration:              Duration::from_secs(30 * 24 * 60 * 60),
            interval_secs:         DEFAULT_INTERVAL_SECS,
            max_evictions_per_run: DEFAULT_MAX_PER_RUN,
            dry_run:               false,
            reload_drain_after_evict: true,
            drain_load_duration:   None,
            quorum_check_enabled:  false,
            quorum_min_peers:      1,
        }
    }

    fn to_retention(&self) -> RetentionConfig {
        RetentionConfig {
            enabled:               self.enabled,
            duration:              self.duration,
            max_evictions_per_run: self.max_evictions_per_run,
            dry_run:                self.dry_run,
            quorum_check_enabled:  self.quorum_check_enabled,
            quorum_min_peers:      self.quorum_min_peers,
        }
    }
}

/// Handle returned by [`start`].  Drop or call [`Handle::stop`] to
/// terminate the task.  Idempotent on disabled handles.
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
                log::error!("[retention] task panicked on shutdown: {e:?}");
            }
        }
    }
}

/// Spawn the periodic retention sweeper.  Returns a no-op handle when:
///
/// - `retention.enabled = false` (the common case — opt-in feature)
/// - `retention.interval_secs = 0` (operators want manual sweeps only)
///
/// In both no-op cases the operator can still trigger sweeps on demand
/// via the `v2/retention.sweep` RPC, which calls `evict_expired`
/// directly without going through this task.
pub fn start(cfg: Config) -> Handle {
    // Always publish the parsed config so the JSON-RPC layer can echo
    // it from `v2/retention.settings` regardless of whether the
    // background task ends up actually running.
    install_config(&cfg);

    if !cfg.enabled {
        log::info!("[retention] disabled (retention.enabled=false) — \
                    use `bdscmd retention-sweep --force` for manual sweeps");
        return Handle::disabled();
    }
    if cfg.interval_secs == 0 {
        log::info!("[retention] enabled but interval_secs=0 — \
                    background sweeper not running, manual sweeps only");
        return Handle::disabled();
    }

    let retention_cfg = cfg.to_retention();
    let drain_reload = cfg.reload_drain_after_evict.then(|| {
        cfg.drain_load_duration.clone().unwrap_or_else(|| "24h".to_owned())
    });

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(run(
        cfg.interval_secs,
        retention_cfg,
        drain_reload,
        shutdown_rx,
    ));
    log::info!(
        "[retention] started — duration={:?} interval={}s max_per_run={} \
         dry_run={} quorum={} min_peers={}",
        cfg.duration, cfg.interval_secs, cfg.max_evictions_per_run, cfg.dry_run,
        cfg.quorum_check_enabled, cfg.quorum_min_peers,
    );
    Handle { shutdown_tx: Some(shutdown_tx), task: Some(task) }
}

async fn run(
    interval_secs:  u64,
    retention_cfg:  RetentionConfig,
    drain_reload:   Option<String>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let interval = Duration::from_secs(interval_secs);
    bdslib::health::register("retention", interval_secs.saturating_mul(3).max(120));
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                log::debug!("[retention] shutdown signal received — stopping");
                break;
            }
            _ = tokio::time::sleep(interval) => {
                bdslib::health::heartbeat("retention");
                run_one_tick(&retention_cfg, drain_reload.as_deref()).await;
            }
        }
    }
}

/// One sweep cycle.  Background-loop entry point — calls
/// [`run_sweep`] and discards the report (it's already recorded via
/// `record_run` inside).
pub async fn run_one_tick(retention_cfg: &RetentionConfig, drain_reload: Option<&str>) {
    let _ = run_sweep(retention_cfg, drain_reload).await;
}

/// Run one quorum-aware retention sweep and return the resulting
/// [`bdslib::retention::EvictionReport`].  Used by both the
/// background loop and the manual `v2/retention.sweep` RPC so quorum
/// semantics are identical between the two.
///
/// `drain_reload` — when `Some`, also re-seed the drain parser after
/// the sweep evicted ≥ 1 shard.  Pass `None` to skip the reload step
/// (useful for dry-run-style manual sweeps that don't change state).
///
/// Errors at the sweep level (catalog unreachable etc.) are returned
/// as the function's `Err`; per-shard failures land in the report's
/// `errors` count and never abort the sweep.
pub async fn run_sweep(
    retention_cfg: &RetentionConfig,
    drain_reload:  Option<&str>,
) -> anyhow::Result<bdslib::retention::EvictionReport> {
    let started = std::time::Instant::now();

    // Phase 3 — pre-fetch peer shard catalogs when quorum gating is
    // on AND we're in cluster mode with at least one Alive peer.
    let quorum_map = if retention_cfg.quorum_check_enabled {
        match fetch_peer_shards().await {
            Ok(Some(m)) => Some(m),
            Ok(None) => {
                // Cluster disabled or no Alive peers → fail safe:
                // refuse every eviction this sweep by handing the
                // closure an empty map.  The library logs each skip.
                log::warn!(
                    "[retention] quorum_check_enabled=true but no Alive \
                     peers available; this sweep will skip all candidates"
                );
                Some(HashMap::new())
            }
            Err(e) => {
                log::warn!("[retention] quorum pre-fetch failed: {e}; \
                            falling back to skip-everything for safety");
                Some(HashMap::new())
            }
        }
    } else {
        None
    };
    let quorum_map_arc = quorum_map.map(Arc::new);

    let cfg     = retention_cfg.clone();
    let reload  = drain_reload.map(str::to_owned);
    let qm      = quorum_map_arc.clone();
    let min_peers = cfg.quorum_min_peers;

    let join = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let closure = move |start_ts: i64, end_ts: i64| -> bool {
            match &qm {
                None    => true,
                Some(m) => m.get(&(start_ts, end_ts)).copied().unwrap_or(0) >= min_peers,
            }
        };
        let report = evict_expired_with_quorum(&cfg, std::time::SystemTime::now(), closure)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        record_run(&report);
        if report.evicted > 0 {
            if let Some(dur) = reload {
                if let Ok(db) = bdslib::get_db() {
                    match db.drain_reload(&dur) {
                        Ok(n) => log::info!(
                            "[retention] drain parser reloaded — {n} cluster(s) from {dur} window"),
                        Err(e) => log::warn!(
                            "[retention] drain reload failed: {e}"),
                    }
                }
            }
        }
        log::debug!("[retention] sweep completed in {:?}", started.elapsed());
        Ok(report)
    }).await
        .map_err(|e| anyhow::anyhow!("retention sweep task panicked: {e}"))??;
    Ok(join)
}

/// Fan out `v2/cluster.shards.list` to every Alive peer.  Returns
/// `Some(map)` where the map keys are `(start_ts, end_ts)` Unix-second
/// tuples and values are "number of peers that hold a shard for this
/// interval".  Returns `Ok(None)` when cluster mode is disabled or no
/// peer is Alive — the caller fail-safes to skip-everything in that
/// case.
async fn fetch_peer_shards() -> anyhow::Result<Option<HashMap<(i64, i64), usize>>> {
    let cluster = match bdslib::get_db().ok().and_then(|d| d.cluster().cloned()) {
        Some(c) => c,
        None    => return Ok(None),  // standalone mode
    };
    if cluster.peers.read().alive().is_empty() {
        return Ok(None);
    }

    let fan = bdslib::cluster::fanout::fan_out_v2(
        &cluster, "v2/cluster.shards.list", serde_json::json!({})
    ).await;

    let mut map: HashMap<(i64, i64), usize> = HashMap::new();
    for resp in fan.responses {
        let body = match resp.result {
            Ok(b) => b,
            Err(e) => {
                log::warn!(
                    "[retention] peer {} ({}) shards.list failed: {e}",
                    resp.peer.node_id, resp.peer.url,
                );
                continue;
            }
        };
        let arr = match body.get("shards").and_then(|v| v.as_array()) {
            Some(a) => a,
            None    => continue,
        };
        for s in arr {
            let start = s.get("start_ts").and_then(|v| v.as_i64()).unwrap_or(-1);
            let end   = s.get("end_ts").  and_then(|v| v.as_i64()).unwrap_or(-1);
            if start < 0 || end <= start { continue; }
            *map.entry((start, end)).or_insert(0) += 1;
        }
    }
    Ok(Some(map))
}
