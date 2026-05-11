//! `v3/trends` — cluster-wide statistical trend summary for a single key.
//!
//! Trends output (mean/median/std-dev/anomalies/breakouts) is corpus-
//! relative, so the merge strategy mirrors v3/topics and v3/rca:
//! fan out the v2 method to every Alive peer, then **pick the response
//! with the largest sample count** (`n` field).  In a fully-replicated
//! steady state every peer reports roughly the same `n` and the picks
//! are equivalent; during partial-mode or recovery, picking the richest
//! corpus avoids returning a result derived from a stale subset.

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::fanout;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

#[derive(serde::Deserialize, Clone)]
struct Params {
    #[serde(default)] session: String,
    key: String,
    duration: String,
}

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v3/trends", |params, _ctx, _| async move {
        let p: Params = params.parse()?;

        let p_local = p.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            humantime::parse_duration(&p_local.duration)
                .map_err(|e| rpc_err(-32600, format!("invalid duration {:?}: {e}", p_local.duration)))?;
            let trend = bdslib::TelemetryTrend::query_window(&p_local.key, &p_local.duration)
                .map_err(|e| rpc_err(-32004, e))?;
            serde_json::to_value(&trend)
                .map_err(|e| rpc_err(-32004, format!("serialise: {e}")))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let v2_params = serde_json::json!({
            "session": p.session, "key": p.key, "duration": p.duration,
        });
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/trends", v2_params).await),
            None    => None,
        };

        let mut best: &JsonValue = &local;
        let mut best_n = local.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
        if let Some(f) = &fan {
            for r in f.ok_results() {
                let n = r.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                if n > best_n { best = r; best_n = n; }
            }
        }
        let mut out = best.clone();
        if let Some(obj) = out.as_object_mut() {
            obj.insert("cluster_meta".into(), v3_cluster_meta(fan));
        }
        Ok::<JsonValue, ErrorObject>(out)
    }).unwrap();
}
