//! `v2/scheduler.last_seen` — most-recent execution timestamp for a
//! stored script as seen by **this** node.
//!
//! Used by the cluster-aware Scheduler tick: before firing a script
//! it fans `v2/scheduler.last_seen` out to every Alive peer plus its
//! own scheduler log, takes the max, and skips the fire if the max
//! is within `cluster.scheduler_dedup_window`.
//!
//! Returns `last_executed_at: null` when this node has never run the
//! script, or when cluster mode is disabled (the scheduler log only
//! exists on cluster-enabled nodes).

use super::params::rpc_err;
use jsonrpsee::RpcModule;

#[derive(serde::Deserialize)]
struct Params {
    script_id: String,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/scheduler.last_seen", |params, _ctx, _| async move {
            let p: Params = params.parse()?;
            let script_id = uuid::Uuid::parse_str(&p.script_id)
                .map_err(|e| rpc_err(-32602, format!("invalid script_id: {e}")))?;

            let result = tokio::task::spawn_blocking(move || {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let cluster = match db.cluster() {
                    Some(c) => c,
                    None    => {
                        // Standalone node — no log; report nothing seen.
                        return Ok::<serde_json::Value, jsonrpsee::types::ErrorObject>(
                            serde_json::json!({ "last_executed_at": null })
                        );
                    }
                };
                let last = cluster.scheduler_log.last_executed(script_id)
                    .map_err(|e| rpc_err(-32004, e))?;
                Ok(serde_json::json!({ "last_executed_at": last }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            result
        })
        .unwrap();
}
