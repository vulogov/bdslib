//! `v3/count` — total record count across the cluster.
//!
//! Two modes:
//!
//! - **Default (`distinct: false`):** sums per-peer `v2/count` results.
//!   Cheap.  Overcounts replicated records by approximately
//!   `replication_factor`.
//! - **`distinct: true` (Phase 5):** fans out `v2/primaries` (UUIDs only)
//!   and unions the sets.  Returns the true distinct-record count
//!   regardless of how many replicas exist for each record.  More
//!   expensive — proportional to record count rather than peer count.
//!
//! Accepts the same `TimeWindowParams` (`duration`, `start_ts`+`end_ts`) as
//! `v2/count` and forwards them verbatim to peers.

use super::params::{rpc_err, v3_cluster_meta, TimeWindow, TimeWindowParams};
use bdslib::cluster::fanout;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use std::collections::HashSet;

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v3/count", |params, _ctx, _| async move {
            log::debug!("v3/count: start");

            // Parse the same window the v2 method accepts and clone it for
            // the peer fan-out (we re-serialise via JSON below).
            let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
            let p: TimeWindowParams = serde_json::from_value(strip_internal(&raw))
                .map_err(|e| rpc_err(-32602, format!("invalid window: {e}")))?;
            let distinct = raw.get("distinct").and_then(|v| v.as_bool()).unwrap_or(false);
            let window = p.resolve()?;

            if distinct {
                return distinct_count(raw, window).await;
            }

            // ── Sum mode (default) ───────────────────────────────────────
            let local_fut = tokio::task::spawn_blocking(move || local_count(&window));

            let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
            let fan_params = strip_internal(&raw);
            let fanout_fut = async {
                match &cluster {
                    Some(c) => Some(fanout::fan_out_v2(c, "v2/count", fan_params).await),
                    None    => None,
                }
            };

            let (local_res, fan) = tokio::join!(local_fut, fanout_fut);
            let local_count_n: u64 = local_res
                .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??
                .get("count").and_then(|v| v.as_u64()).unwrap_or(0);

            let mut total = local_count_n;
            if let Some(f) = &fan {
                for r in f.ok_results() {
                    total += r.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                }
            }

            log::debug!("v3/count: done (sum total={total})");
            Ok::<JsonValue, ErrorObject>(serde_json::json!({
                "count":        total,
                "local_count":  local_count_n,
                "distinct":     false,
                "cluster_meta": v3_cluster_meta(fan),
            }))
        })
        .unwrap();
}

async fn distinct_count(raw: JsonValue, window: TimeWindow) -> Result<JsonValue, ErrorObject<'static>> {
    // Local UUID list on a blocking thread.
    let local_fut = tokio::task::spawn_blocking(move || local_primary_ids(&window));

    let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
    let fan_params = strip_internal(&raw);
    let fanout_fut = async {
        match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/primaries", fan_params).await),
            None    => None,
        }
    };

    let (local_res, fan) = tokio::join!(local_fut, fanout_fut);
    let local_ids: Vec<String> = local_res
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

    let local_count = local_ids.len() as u64;
    let mut union: HashSet<String> = local_ids.into_iter().collect();
    if let Some(f) = &fan {
        for r in f.ok_results() {
            if let Some(arr) = r.get("ids").and_then(|v| v.as_array()) {
                for x in arr {
                    if let Some(s) = x.as_str() { union.insert(s.to_owned()); }
                }
            }
        }
    }

    let total = union.len() as u64;
    log::debug!("v3/count: done (distinct total={total} local={local_count})");
    Ok(serde_json::json!({
        "count":        total,
        "local_count":  local_count,
        "distinct":     true,
        "cluster_meta": v3_cluster_meta(fan),
    }))
}

fn strip_internal(raw: &JsonValue) -> JsonValue {
    let mut out = raw.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.remove("distinct");
    }
    out
}

fn local_primary_ids(window: &TimeWindow) -> Result<Vec<String>, ErrorObject<'static>> {
    let db    = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
    let cache = db.cache();
    let shard_infos = match window {
        TimeWindow::All => cache.info().list_all(),
        TimeWindow::Range(s, e) => cache.info().shards_in_range(*s, *e),
    }.map_err(|e| rpc_err(-32002, e))?;

    let mut ids: Vec<String> = Vec::new();
    for si in shard_infos {
        let shard = cache.shard(si.start_time).map_err(|e| rpc_err(-32003, e))?;
        let obs = shard.observability();
        let uuids = match window {
            TimeWindow::All => obs.list_primaries(),
            TimeWindow::Range(s, e) => obs.list_primaries_in_range(*s, *e),
        }.map_err(|e| rpc_err(-32004, e))?;
        ids.extend(uuids.into_iter().map(|u| u.to_string()));
    }
    Ok(ids)
}

fn local_count(window: &TimeWindow) -> Result<JsonValue, ErrorObject<'static>> {
    let db    = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
    let cache = db.cache();
    let shard_infos = match window {
        TimeWindow::All => cache.info().list_all(),
        TimeWindow::Range(s, e) => cache.info().shards_in_range(*s, *e),
    }.map_err(|e| rpc_err(-32002, e))?;

    let mut total: u64 = 0;
    for si in shard_infos {
        let s = cache.shard(si.start_time).map_err(|e| rpc_err(-32003, e))?;
        let n = match window {
            TimeWindow::All => s.observability().count_all(),
            TimeWindow::Range(start, end) => s.observability().count_in_range(*start, *end),
        }.map_err(|e| rpc_err(-32004, e))?;
        total += n;
    }
    Ok(serde_json::json!({ "count": total }))
}
