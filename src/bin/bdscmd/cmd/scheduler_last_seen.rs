use anyhow::Result;
use clap::Args;
use serde_json::Value;

/// Read the most-recent execution timestamp the **target node** has
/// recorded for a stored script.  Returns `last_executed_at: null`
/// when this node has never run the script, when the node is in
/// standalone mode (no cluster scheduler log), or when the cluster-
/// aware scheduler hasn't fired the script yet.
///
/// Used by the cluster-aware Scheduler internally to suppress
/// duplicate fires across the cluster; exposed here as a CLI so
/// operators can verify dedup is working without tailing logs.
#[derive(Args)]
pub struct Cmd {
    /// UUIDv7 of the stored script (the same id returned by
    /// `script-add` / `bdscmd scripts`).
    script_id: String,
}

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    crate::client::call(
        url,
        "v2/scheduler.last_seen",
        serde_json::json!({ "script_id": args.script_id }),
    )
}
