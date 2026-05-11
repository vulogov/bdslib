//! `v3/search.get` — cluster-wide semantic search returning **full
//! documents** (companion to `v3/search` which returns ids+scores).
//!
//! UUID dedup; if the same UUID appears on multiple peers (because
//! Phase 3 sharded replication put it on more than one node), the first
//! peer's full document is kept and the score is averaged.

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::{fanout, merge};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

fn default_limit() -> usize { 10 }

#[derive(serde::Deserialize, Clone)]
struct Params {
    #[serde(default)] session: String,
    query: String,
    duration: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v3/search.get", |params, _ctx, _| async move {
        let p: Params = params.parse()?;
        let v2_params = serde_json::json!({
            "session": p.session, "query": p.query.clone(),
            "duration": p.duration.clone(), "limit": p.limit,
        });

        let p_local = p.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let query_json = serde_json::json!(p_local.query);
            let docs = db.vectorsearch_recent(&p_local.duration, &query_json, p_local.limit)
                .map_err(|e| rpc_err(-32004, e))?;
            Ok(serde_json::json!({ "results": docs }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/search.get", v2_params).await),
            None    => None,
        };
        // Use score-aware dedup so duplicate replicas don't outrank singletons.
        let bodies = merge::bodies_from(&local, fan.as_ref());
        let mut merged = merge::dedup_avg_score(bodies, "results");
        merged.truncate(p.limit);

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "results": merged, "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}
