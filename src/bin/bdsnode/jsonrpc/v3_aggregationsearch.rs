//! `v3/aggregationsearch` — cluster-wide combined telemetry vector
//! search + document-store semantic search.
//!
//! Local + fan-out v2/aggregationsearch.  Each response is two arrays
//! (`observability` + `documents`); both are merged by UUID dedup with
//! score average (same shape used by v3/search.get).

use super::params::{rpc_err, v3_cluster_meta};
use super::v3_merge;
use bdslib::cluster::fanout;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

#[derive(serde::Deserialize, Clone)]
struct Params {
    #[serde(default)] session: String,
    duration: String,
    query: String,
}

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v3/aggregationsearch", |params, _ctx, _| async move {
        let p: Params = params.parse()?;

        let p_local = p.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.aggregationsearch(&p_local.duration, &p_local.query)
                .map_err(|e| rpc_err(-32004, e))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let v2_params = serde_json::json!({
            "session": p.session, "duration": p.duration, "query": p.query,
        });
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/aggregationsearch", v2_params).await),
            None    => None,
        };

        let mut bodies: Vec<&JsonValue> = vec![&local];
        if let Some(f) = &fan { bodies.extend(f.ok_results()); }
        let observability = v3_merge::dedup_avg_score(bodies.clone(), "observability");
        let documents     = v3_merge::dedup_avg_score(bodies,         "documents");

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "observability": observability,
            "documents":     documents,
            "cluster_meta":  v3_cluster_meta(fan),
        }))
    }).unwrap();
}
