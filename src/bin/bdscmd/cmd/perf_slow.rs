//! `bdscmd perf-slow` — fetch the slow-query log (`v2/perf.slow_queries`).
//!
//! Every `perf::time` call participates: when its elapsed time exceeds
//! the process-wide threshold (`perf.slow_query_threshold_ms` in
//! bds.hjson, default 500 ms), it lands in a bounded 100-entry ring.
//! Use this command to spot outliers that p95 doesn't surface.

use anyhow::Result;
use clap::Args;
use serde_json::Value;

#[derive(Args)]
pub struct Cmd {
    /// Only return entries whose series name starts with this prefix
    /// (e.g. `fanout.`, `ingest.`, `v3/`).
    #[arg(long, default_value = "")]
    name_prefix: String,

    /// Only return entries no older than this many seconds.  `0`
    /// (default) returns every entry in the ring.
    #[arg(long, default_value_t = 0)]
    since_secs: u64,
}

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    crate::client::call(url, "v2/perf.slow_queries", serde_json::json!({
        "name_prefix": args.name_prefix,
        "since_secs":  args.since_secs,
    }))
}
