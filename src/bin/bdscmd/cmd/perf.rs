//! `bdscmd perf` — print the full v2/perf snapshot from a node.
//!
//! Plain `bdscmd perf` returns the full registry (one entry per named
//! series with p50/p95/p99/n_total/...).  Pass `--name <prefix>` to
//! filter; the filter is a substring match, so `--name ingest` keeps
//! ingest.* while `--name fanout.peer.` keeps only the per-peer
//! fan-out series.

use anyhow::Result;
use clap::Args;
use serde_json::Value;

#[derive(Args)]
pub struct Cmd {
    /// Substring filter applied to series names (e.g. "ingest",
    /// "fanout.peer.", "replicate.method.v2/add").  Empty = show all.
    #[arg(long, default_value = "")]
    name: String,
}

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    let full = crate::client::call(url, "v2/perf", serde_json::json!({}))?;
    if args.name.is_empty() {
        return Ok(full);
    }
    let needle = args.name;
    if let Value::Object(map) = full {
        let filtered: serde_json::Map<String, Value> = map.into_iter()
            .filter(|(k, _)| k.contains(&needle))
            .collect();
        Ok(Value::Object(filtered))
    } else {
        Ok(full)
    }
}
