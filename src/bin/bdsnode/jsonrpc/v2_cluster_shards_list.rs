//! `v2/cluster.shards.list` — pure catalog dump for Phase 3 retention
//! quorum probes.
//!
//! Returns `(shard_id, start_ts, end_ts)` tuples straight from
//! [`ShardInfoEngine::list_all`] without opening any shard.  Designed
//! to be cheap enough that a peer can call it once per Alive sibling
//! at the start of each retention sweep to decide whether eviction
//! candidates have safe replicas elsewhere.
//!
//! Unauthenticated v2/* read — same trust boundary as
//! `v2/cluster.peers` / `v2/timeline`.
//!
//! [`ShardInfoEngine::list_all`]: bdslib::shardsinfo::ShardInfoEngine::list_all

use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::{json, Value as JsonValue};
use std::time::UNIX_EPOCH;

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v2/cluster.shards.list", |_params, _ctx, _| async move {
        let resp = tokio::task::spawn_blocking(|| -> Result<JsonValue, ErrorObject<'static>> {
            let db   = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let info = db.cache().info();
            let rows = info.list_all().map_err(|e| rpc_err(-32002, e))?;

            // Mirror the cluster wire convention: emit Unix seconds
            // for both bounds so the caller can do exact-interval
            // matching against its own catalog without re-deriving
            // from SystemTime.
            let shards: Vec<JsonValue> = rows.into_iter().map(|s| {
                let start_ts = s.start_time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let end_ts   = s.end_time  .duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                json!({
                    "shard_id": s.shard_id.to_string(),
                    "start_ts": start_ts,
                    "end_ts":   end_ts,
                })
            }).collect();

            Ok(json!({
                "n_shards": shards.len(),
                "shards":   shards,
            }))
        }).await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;
        Ok::<JsonValue, ErrorObject>(resp)
    }).unwrap();
}
