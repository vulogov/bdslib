//! `bdscmd health` — node readiness / liveness probe (`v2/health`).
//!
//! Returns the aggregate self-healing verdict
//! (`healthy` / `degraded` / `failed`) plus the per-source breakdown:
//! every background-task heartbeat, the ingest-flusher supervisor, and
//! one entry per quarantined shard.  A stale heartbeat (a hung loop)
//! shows up as `failed` with `stale: true`.
//!
//! Designed for orchestrators and load balancers — cheap, in-process,
//! no DB access.  With `--quiet`, prints only the one-word verdict and
//! exits non-zero when the verdict is not `healthy`, which makes it
//! usable directly as a shell health check.

use anyhow::Result;
use clap::Args;
use serde_json::Value;

#[derive(Args)]
pub struct Cmd {
    /// Print only the one-word verdict (`healthy`/`degraded`/`failed`)
    /// and exit non-zero when it is not `healthy` — for use as a
    /// scriptable health check.
    #[arg(long)]
    quiet: bool,
}

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    let resp = crate::client::call(url, "v2/health", serde_json::json!({}))?;

    if args.quiet {
        // Print just the verdict and exit immediately — bypassing
        // main's pretty-printer — so this is a clean scriptable probe:
        // exit 0 when healthy, exit 1 otherwise.
        let status = resp.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
        println!("{status}");
        std::process::exit(if status == "healthy" { 0 } else { 1 });
    }

    Ok(resp)
}
