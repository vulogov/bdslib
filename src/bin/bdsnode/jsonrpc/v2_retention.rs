//! `v2/retention.sweep` + `v2/retention.settings` — operator-triggered
//! shard eviction sweep and read-only echo of the active retention
//! configuration.
//!
//! `sweep` reuses [`bdslib::retention::evict_expired`] — same code
//! path the background tokio task drives.  Useful for:
//!
//! - Verifying a config change before flipping `retention.enabled=true`
//!   in `bds.hjson` (pass `dry_run: true`).
//! - Reclaiming disk now instead of waiting for the next interval.
//! - Driving sweeps from a CI integration test.
//!
//! Both methods are unauthenticated v2/* — the bdsnode RPC port is
//! the trust boundary, matching `v2/eval` / `v2/doc.*` conventions.
//! Operators who want HMAC on retention should run bdsnode behind a
//! reverse proxy that enforces it (or call from a cluster admin
//! script over the loopback interface).

use super::params::rpc_err;
use bdslib::retention::RetentionConfig;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use std::time::Duration;

pub fn register(module: &mut RpcModule<()>) {
    register_sweep(module);
    register_settings(module);
}

#[derive(Deserialize, Default)]
struct SweepParams {
    /// Override `retention.duration` for this one call (humantime).
    /// Omit to use the configured value.
    #[serde(default)]
    duration: Option<String>,

    /// Override `retention.max_evictions_per_run` for this one call.
    /// `0` = no cap.
    #[serde(default)]
    max_evictions_per_run: Option<u64>,

    /// Log evictions without acting on them.  Defaults to whatever
    /// `retention.dry_run` is in `bds.hjson`.
    #[serde(default)]
    dry_run: Option<bool>,

    /// Force-enable the sweeper for this call even when
    /// `retention.enabled = false` in `bds.hjson`.  Useful for
    /// manual cleanup on a node that's normally read-mostly.
    #[serde(default)]
    force: bool,
}

fn register_sweep(module: &mut RpcModule<()>) {
    module.register_async_method("v2/retention.sweep", |params, _ctx, _| async move {
        let p: SweepParams = if let Ok(opt) = params.parse::<Option<SweepParams>>() {
            opt.unwrap_or_default()
        } else {
            SweepParams::default()
        };

        // Read the active runtime config (installed by
        // bdsnode/main.rs via `server::retention::start`).  Falling
        // back to library defaults ({enabled: false, duration: 30d})
        // keeps test harnesses safe.
        let (mut lib_cfg, drain_dur) = match crate::server::retention::active() {
            Some(a) => (RetentionConfig {
                enabled:               a.enabled,
                duration:              a.duration,
                max_evictions_per_run: a.max_evictions_per_run,
                dry_run:                a.dry_run,
                quorum_check_enabled:  a.quorum_check_enabled,
                quorum_min_peers:      a.quorum_min_peers,
            }, a.drain_load_duration.clone()),
            None => (RetentionConfig {
                enabled:               false,
                duration:              std::time::Duration::from_secs(30 * 24 * 60 * 60),
                max_evictions_per_run: 50,
                dry_run:                false,
                quorum_check_enabled:  false,
                quorum_min_peers:      1,
            }, None),
        };

        if p.force {
            lib_cfg.enabled = true;
        }
        if let Some(dur_str) = &p.duration {
            let d = humantime::parse_duration(dur_str)
                .map_err(|e| rpc_err(-32600, format!("retention.sweep: bad duration {dur_str:?}: {e}")))?;
            if d.is_zero() {
                return Err(rpc_err(-32600, "retention.sweep: duration must be > 0"));
            }
            lib_cfg.duration = d;
        }
        if let Some(n) = p.max_evictions_per_run {
            lib_cfg.max_evictions_per_run = n as usize;
        }
        if let Some(dr) = p.dry_run {
            lib_cfg.dry_run = dr;
        }

        // Delegate to `server::retention::run_sweep` so manual sweeps
        // and the background task share identical quorum semantics.
        // Drain reload is suppressed in dry-run mode.
        let reload = (!lib_cfg.dry_run).then(|| drain_dur.clone()).flatten();
        let report = crate::server::retention::run_sweep(&lib_cfg, reload.as_deref())
            .await
            .map_err(|e| rpc_err(-32004, format!("retention.sweep: {e}")))?;

        Ok::<JsonValue, ErrorObject>(json!({
            "enabled":         lib_cfg.enabled,
            "duration_secs":   lib_cfg.duration.as_secs(),
            "dry_run":         report.dry_run,
            "disabled":        report.disabled,
            "evicted":         report.evicted,
            "errors":          report.errors,
            "freed_bytes":     report.freed_bytes,
            "cutoff_ts":       report.cutoff_ts,
            "took_ms":         report.took_ms,
            "min_start_ts":    report.min_start_ts,
            "max_end_ts":      report.max_end_ts,
            "quorum_skipped":  report.quorum_skipped,
            "quorum_enabled":  lib_cfg.quorum_check_enabled,
        }))
    }).unwrap();
}

fn register_settings(module: &mut RpcModule<()>) {
    module.register_async_method("v2/retention.settings", |_params, _ctx, _| async move {
        // Echo the active runtime config + lifetime stats.  Read from
        // the OnceLock installed by `server::retention::start` so the
        // values reflect what bdsnode is actually running with — not
        // a re-parse of `$BDS_CONFIG`, which won't match a node
        // started via `--config <path>`.
        use std::sync::atomic::Ordering;
        let s = bdslib::retention::stats();
        let stats_block = json!({
            "evicted_lifetime":         s.evicted_lifetime.load(Ordering::Relaxed),
            "evicted_last_run":         s.evicted_last_run.load(Ordering::Relaxed),
            "freed_lifetime_bytes":     s.freed_lifetime_bytes.load(Ordering::Relaxed),
            "freed_last_run_bytes":     s.freed_last_run_bytes.load(Ordering::Relaxed),
            "last_run_ts":              s.last_run_ts.load(Ordering::Relaxed),
            "last_run_ms":              s.last_run_ms.load(Ordering::Relaxed),
            "errors_lifetime":          s.errors_lifetime.load(Ordering::Relaxed),
            "quorum_skipped_lifetime":  s.quorum_skipped_lifetime.load(Ordering::Relaxed),
            "quorum_skipped_last_run":  s.quorum_skipped_last_run.load(Ordering::Relaxed),
        });

        let out = match crate::server::retention::active() {
            Some(a) => json!({
                "installed":              true,
                "enabled":                a.enabled,
                "duration":               format_humantime(a.duration),
                "duration_secs":          a.duration.as_secs(),
                "interval_secs":          a.interval_secs,
                "max_evictions_per_run":  a.max_evictions_per_run,
                "dry_run":                 a.dry_run,
                "reload_drain_after_evict": a.reload_drain_after_evict,
                "drain_load_duration":    a.drain_load_duration,
                "quorum_check_enabled":   a.quorum_check_enabled,
                "quorum_min_peers":       a.quorum_min_peers,
                "stats":                  stats_block,
            }),
            None => json!({
                // Test-binary / partial-init case — no
                // server::retention::start has run, so we have no
                // active config to echo.
                "installed":             false,
                "stats":                 stats_block,
            }),
        };
        Ok::<JsonValue, ErrorObject>(out)
    }).unwrap();
}

fn format_humantime(d: Duration) -> String {
    // humantime::format_duration is the canonical "30days 1h 5m"-style
    // serialiser — operators read this in v2/status / settings and we
    // want it to match `bds.hjson` exactly when the config is canonical.
    humantime::format_duration(d).to_string()
}
