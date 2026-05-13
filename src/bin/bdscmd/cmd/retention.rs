//! `bdscmd retention-sweep` / `retention-settings` — drive the
//! `v2/retention.*` surface from the shell.
//!
//! `sweep` triggers one-shot eviction; pass `--dry-run` to preview
//! without acting.  Useful for verifying a policy change before
//! flipping `retention.enabled=true` in `bds.hjson`, or reclaiming
//! disk on demand instead of waiting for the next interval tick.
//!
//! `settings` echoes the effective config + lifetime stats so
//! operators can confirm what's loaded.

use anyhow::Result;
use clap::Args;
use serde_json::{Map, Value};

// ── retention-sweep ───────────────────────────────────────────────────────────

#[derive(Args)]
pub struct SweepCmd {
    /// Override `retention.duration` for this call (humantime, e.g.
    /// `"7days"`, `"6h"`).  Omit to use the configured value.
    #[arg(long)]
    duration: Option<String>,

    /// Override `retention.max_evictions_per_run` for this call.
    /// `0` = no cap.
    #[arg(long)]
    max_evictions_per_run: Option<u64>,

    /// Log what would be evicted without acting.  Overrides
    /// `retention.dry_run` in `bds.hjson`.
    #[arg(long)]
    dry_run: bool,

    /// Force-enable the sweeper for this call even when
    /// `retention.enabled = false` in `bds.hjson`.  Useful for
    /// manual cleanup on a node that's normally read-mostly.
    #[arg(long)]
    force: bool,
}

pub fn sweep(url: &str, _session: &str, args: SweepCmd) -> Result<Value> {
    let mut params = Map::new();
    if let Some(d) = args.duration {
        params.insert("duration".into(), Value::String(d));
    }
    if let Some(n) = args.max_evictions_per_run {
        params.insert("max_evictions_per_run".into(), Value::Number(n.into()));
    }
    if args.dry_run {
        params.insert("dry_run".into(), Value::Bool(true));
    }
    if args.force {
        params.insert("force".into(), Value::Bool(true));
    }
    crate::client::call(url, "v2/retention.sweep", Value::Object(params))
}

// ── retention-settings ────────────────────────────────────────────────────────

#[derive(Args)]
pub struct SettingsCmd;

pub fn settings(url: &str, _session: &str, _args: SettingsCmd) -> Result<Value> {
    crate::client::call(url, "v2/retention.settings", Value::Object(Map::new()))
}
