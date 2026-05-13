//! Background tokio task that periodically generates synthetic
//! telemetry via [`bdslib::common::realistic::generate`] and pushes
//! it through the local `"ingest"` pipe.
//!
//! **Dev/demo only.**  When enabled the node emits a loud startup
//! banner and updates [`bdslib::dev_data::stats`] so every status
//! surface (v2/status, bdsweb dashboard, …) can warn that the
//! displayed data is artificial.
//!
//! Enable via either:
//!
//! - `generate_realistic_data.enabled = true` in `bds.hjson`, or
//! - `--generate_realistic_data` on the bdsnode CLI (overrides
//!   hjson, even when the hjson block is absent or `enabled=false`).
//!
//! Config keys (under `generate_realistic_data:`):
//!
//! | hjson key      | default | description |
//! |----------------|---------|-------------|
//! | `enabled`      | `false` | Master switch.                                                  |
//! | `interval_secs`| `60`    | Seconds between batches.  Clamped to `[5, 86400]`.              |
//! | `duration`     | `"6h"`  | Humantime — passed straight to `RealisticConfig::duration`.     |
//! | `total`        | `2000`  | Records per batch.                                              |
//! | `scenarios`    | `3`     | Incident cascades per batch.                                    |
//! | `noise_ratio`  | `0.7`   | Background-noise fraction.                                      |
//! | `anomaly_ratio`| `0.02`  | Rare-record fraction.                                           |
//! | `seed`         | `null`  | Optional RNG seed for deterministic test datasets.              |

use bdslib::common::realistic::{generate, RealisticConfig};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::oneshot;

const DEFAULT_INTERVAL_SECS: u64 = 60;
const MIN_INTERVAL_SECS:     u64 = 5;
const MAX_INTERVAL_SECS:     u64 = 86_400;

/// Parsed `generate_realistic_data.*` block from `bds.hjson`,
/// possibly overridden by the CLI flag.
pub struct Config {
    pub enabled:        bool,
    pub interval_secs:  u64,
    pub realistic:      RealisticConfig,
}

impl Config {
    /// Read config and optionally force-enable via the CLI flag.
    /// `cli_enable=true` flips `enabled=true` regardless of hjson;
    /// `cli_enable=false` leaves the hjson value alone.
    pub fn from_config(config_path: Option<&str>, cli_enable: bool) -> anyhow::Result<Self> {
        let mut cfg = Self::default_with_cli(cli_enable);

        let path = match config_path {
            Some(p) => p.to_string(),
            None => match std::env::var("BDS_CONFIG") {
                Ok(p) => p,
                Err(_) => return Ok(cfg),  // no file, default applies
            },
        };

        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("cannot read config {path:?}: {e}"))?;
        let val: serde_hjson::Value = serde_hjson::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("hjson parse error in {path:?}: {e}"))?;
        let obj = val.as_object()
            .ok_or_else(|| anyhow::anyhow!("config must be a JSON object"))?;

        let Some(block) = obj.get("generate_realistic_data").and_then(|v| v.as_object()) else {
            // No hjson block — defaults already applied (with the CLI override).
            return Ok(cfg);
        };

        if let Some(b) = block.get("enabled").and_then(|v| v.as_bool()) {
            cfg.enabled = b;
        }
        // CLI flag wins over the hjson value, regardless of order.
        if cli_enable {
            cfg.enabled = true;
        }

        if let Some(n) = block.get("interval_secs").and_then(|v| v.as_f64()) {
            cfg.interval_secs = (n as u64).clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);
        }
        if let Some(s) = block.get("duration").and_then(|v| v.as_str()) {
            cfg.realistic.duration = s.to_owned();
        }
        if let Some(n) = block.get("total").and_then(|v| v.as_f64()) {
            cfg.realistic.total = (n as usize).max(1);
        }
        if let Some(n) = block.get("scenarios").and_then(|v| v.as_f64()) {
            cfg.realistic.scenarios = n as usize;
        }
        if let Some(n) = block.get("noise_ratio").and_then(|v| v.as_f64()) {
            cfg.realistic.noise_ratio = n;
        }
        if let Some(n) = block.get("anomaly_ratio").and_then(|v| v.as_f64()) {
            cfg.realistic.anomaly_ratio = n;
        }
        if let Some(n) = block.get("seed").and_then(|v| v.as_f64()) {
            cfg.realistic.seed = Some(n as u64);
        }

        Ok(cfg)
    }

    fn default_with_cli(cli_enable: bool) -> Self {
        Self {
            enabled:       cli_enable,  // false unless CLI forces it
            interval_secs: DEFAULT_INTERVAL_SECS,
            realistic:     RealisticConfig::default(),
        }
    }
}

/// Slim snapshot for the JSON-RPC reflection surface.
#[derive(Debug, Clone)]
pub struct ActiveConfig {
    pub enabled:        bool,
    pub interval_secs:  u64,
    pub duration:       String,
    pub total:          usize,
    pub scenarios:      usize,
    pub noise_ratio:    f64,
    pub anomaly_ratio:  f64,
    pub seed:           Option<u64>,
}

static ACTIVE: OnceLock<ActiveConfig> = OnceLock::new();

/// Borrow the active config installed by [`start`].  Returns `None`
/// when `start` was never called.
pub fn active() -> Option<&'static ActiveConfig> {
    ACTIVE.get()
}

fn install(cfg: &Config) {
    let _ = ACTIVE.set(ActiveConfig {
        enabled:       cfg.enabled,
        interval_secs: cfg.interval_secs,
        duration:      cfg.realistic.duration.clone(),
        total:         cfg.realistic.total,
        scenarios:     cfg.realistic.scenarios,
        noise_ratio:   cfg.realistic.noise_ratio,
        anomaly_ratio: cfg.realistic.anomaly_ratio,
        seed:          cfg.realistic.seed,
    });
}

/// Handle returned by [`start`].  Drop or call [`Handle::stop`] to
/// terminate.  Idempotent on disabled handles.
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
                log::error!("[dev_data] task panicked on shutdown: {e:?}");
            }
        }
    }
}

/// Spawn the background generator.  No-op when `cfg.enabled = false`.
///
/// When enabled, emits the multi-line banner from
/// [`bdslib::dev_data::loud_warning_banner`] to WARN-level logs so
/// operators can never mistake a demo node for a real one, then
/// publishes the active config so `v2/status` can echo it.
pub fn start(cfg: Config) -> Handle {
    install(&cfg);
    if !cfg.enabled {
        log::debug!("[dev_data] disabled (generate_realistic_data.enabled=false, --generate_realistic_data not set)");
        return Handle::disabled();
    }

    // Loud, multi-line WARN banner.  Centralised in the library so
    // bdsweb / bdscmd can render the exact same message.
    for line in bdslib::dev_data::loud_warning_banner().lines() {
        log::warn!("{line}");
    }
    log::warn!(
        "[dev_data] generator armed — interval={}s duration={:?} \
         total={} scenarios={} noise={} anomaly={}",
        cfg.interval_secs, cfg.realistic.duration, cfg.realistic.total,
        cfg.realistic.scenarios, cfg.realistic.noise_ratio, cfg.realistic.anomaly_ratio,
    );

    bdslib::dev_data::mark_enabled();

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg.interval_secs, cfg.realistic.clone(), shutdown_rx));
    Handle { shutdown_tx: Some(shutdown_tx), task: Some(task) }
}

async fn run(
    interval_secs:  u64,
    realistic_cfg:  RealisticConfig,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let interval = Duration::from_secs(interval_secs);

    // Emit the first batch immediately so dashboards have something
    // to render within seconds of start, not after a full interval.
    emit_one_batch(&realistic_cfg).await;

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                log::debug!("[dev_data] shutdown signal received");
                break;
            }
            _ = tokio::time::sleep(interval) => {
                emit_one_batch(&realistic_cfg).await;
            }
        }
    }
}

async fn emit_one_batch(realistic_cfg: &RealisticConfig) {
    let cfg = realistic_cfg.clone();
    let join = tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        let docs = generate(&cfg);
        let n = docs.len();
        // Push through the same "ingest" channel that v2/add.batch
        // uses — this exercises the full ingest path (sharding +
        // FTS + vector + drain) just like real production traffic.
        match bdslib::pipe::send_many("ingest", docs) {
            Ok(()) => {
                let ms = started.elapsed().as_millis() as u64;
                bdslib::dev_data::record_batch(n, ms);
                log::info!(
                    "[dev_data] enqueued batch — n={n} took={ms}ms"
                );
            }
            Err(e) => {
                bdslib::dev_data::record_error();
                log::warn!("[dev_data] enqueue failed: {e}");
            }
        }
    }).await;
    if let Err(e) = join {
        bdslib::dev_data::record_error();
        log::error!("[dev_data] batch task panicked: {e:?}");
    }
}
