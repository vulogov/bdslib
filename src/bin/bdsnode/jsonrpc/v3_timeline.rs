//! `v3/timeline` — earliest and latest timestamps across the cluster.
//!
//! Calls the local v2/timeline implementation and `v2/timeline` on every
//! Alive peer in parallel, then takes `min(min_ts)` and `max(max_ts)` over
//! all responses.  Falls back to local-only when cluster mode is disabled
//! or the cluster has no Alive peers.

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::fanout;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v3/timeline", |_params, _ctx, _| async move {
            log::debug!("v3/timeline: start");

            // Local timeline runs on a blocking thread (DuckDB).
            let local_fut = tokio::task::spawn_blocking(local_timeline);

            // Fan-out to peers in parallel (only when cluster mode is on).
            let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
            let fanout_fut = async {
                match &cluster {
                    Some(c) => Some(fanout::fan_out_v2(c, "v2/timeline", serde_json::json!({})).await),
                    None    => None,
                }
            };

            let (local_res, fan) = tokio::join!(local_fut, fanout_fut);
            let local = local_res
                .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

            // Merge: min over all min_ts, max over all max_ts.
            let mut acc_min: Option<i64> = local.get("min_ts").and_then(|v| v.as_i64());
            let mut acc_max: Option<i64> = local.get("max_ts").and_then(|v| v.as_i64());

            if let Some(f) = &fan {
                for r in f.ok_results() {
                    let m = r.get("min_ts").and_then(|v| v.as_i64());
                    let x = r.get("max_ts").and_then(|v| v.as_i64());
                    acc_min = match (acc_min, m) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (None,    b)       => b,
                        (a,       None)    => a,
                    };
                    acc_max = match (acc_max, x) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (None,    b)       => b,
                        (a,       None)    => a,
                    };
                }
            }

            log::debug!("v3/timeline: done");
            Ok::<JsonValue, ErrorObject>(serde_json::json!({
                "min_ts":       acc_min,
                "max_ts":       acc_max,
                "cluster_meta": v3_cluster_meta(fan),
            }))
        })
        .unwrap();
}

/// Local v2/timeline equivalent, run from `spawn_blocking` so we don't block
/// the tokio runtime on DuckDB.
fn local_timeline() -> Result<JsonValue, ErrorObject<'static>> {
    let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
    let cache  = db.cache();
    let shards = cache.info().list_all().map_err(|e| rpc_err(-32002, e))?;

    let mut min_ts: Option<i64> = None;
    let mut max_ts: Option<i64> = None;

    for info in &shards {
        let s = cache.shard(info.start_time).map_err(|e| rpc_err(-32003, e))?;
        let (smin, _) = s.observability().timestamp_range().map_err(|e| rpc_err(-32004, e))?;
        if smin.is_some() { min_ts = smin; break; }
    }
    for info in shards.iter().rev() {
        let s = cache.shard(info.start_time).map_err(|e| rpc_err(-32003, e))?;
        let (_, smax) = s.observability().timestamp_range().map_err(|e| rpc_err(-32004, e))?;
        if smax.is_some() { max_ts = smax; break; }
    }

    Ok(serde_json::json!({ "min_ts": min_ts, "max_ts": max_ts }))
}

