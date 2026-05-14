//! `v3/search` — semantic vector search across the cluster.
//!
//! Each peer runs `v2/search` against its own corpus; the coordinator
//! merges the per-peer hit lists, dedups by UUID (first-seen wins), sorts
//! by score (descending), and truncates to `limit`.
//!
//! When the same record is replicated to multiple peers (Phase 3+), every
//! peer's score for that record is averaged into a single representative
//! score per UUID — so duplicated replicas never inflate the rank order.

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::{fanout, merge};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

fn default_limit() -> usize { 10 }

#[derive(serde::Deserialize, Clone)]
struct SearchParams {
    #[serde(default)]
    session: String,
    query: String,
    duration: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v3/search", |params, _ctx, _| async move {
            log::debug!("v3/search: start");
            let p: SearchParams = params.parse()?;

            // Embed the query exactly once on the coordinator and ship
            // the vector to every peer.  Without this, every peer
            // re-runs ONNX inference for the same string — N+1 embed
            // calls per cluster query.  The embed runs on a blocking
            // pool because it's CPU-bound.
            let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
            let query_vec_opt: Option<Vec<f32>> = if cluster.is_some() {
                let q = p.query.clone();
                tokio::task::spawn_blocking(move || -> Option<Vec<f32>> {
                    let db   = bdslib::get_db().ok()?;
                    let qjson = serde_json::json!(q);
                    let fp   = bdslib::json_fingerprint(&qjson);
                    db.cache().embedding().embed(&fp).ok()
                })
                .await
                .ok()
                .flatten()
            } else { None };

            // Fan-out forwards exactly the same shape the local v2/search
            // accepts.  When we've pre-embedded the query, the peer's
            // v2/search skips its own ONNX call.
            let mut v2_params = serde_json::json!({
                "session":  p.session,
                "query":    p.query,
                "duration": p.duration,
                "limit":    p.limit,
            });
            if let Some(ref qv) = query_vec_opt {
                if let Some(obj) = v2_params.as_object_mut() {
                    obj.insert("query_vec".to_owned(), serde_json::json!(qv));
                }
            }

            let p_local = p.clone();
            let qv_local = query_vec_opt.clone();
            let local_fut = tokio::task::spawn_blocking(move || local_search(&p_local, qv_local));

            let fanout_fut = async {
                match &cluster {
                    Some(c) => Some(fanout::fan_out_v2(c, "v2/search", v2_params).await),
                    None    => None,
                }
            };

            let (local_res, fan) = tokio::join!(local_fut, fanout_fut);
            let local = local_res
                .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

            // dedup_avg_score handles UUID dedup + score average + replicas
            // counting + score-desc sort in a single call.
            let bodies = merge::bodies_from(&local, fan.as_ref());
            let mut results = merge::dedup_avg_score(bodies, "results");
            results.truncate(p.limit);

            log::debug!("v3/search: done (n={})", results.len());
            Ok::<JsonValue, ErrorObject>(serde_json::json!({
                "results":      results,
                "cluster_meta": v3_cluster_meta(fan),
            }))
        })
        .unwrap();
}

fn local_search(p: &SearchParams, query_vec: Option<Vec<f32>>)
    -> Result<JsonValue, ErrorObject<'static>>
{
    let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
    let query_json = serde_json::json!(p.query);
    let hits = match query_vec {
        Some(qv) => {
            let fp = bdslib::json_fingerprint(&query_json);
            db.vectorsearch_with_vec(&p.duration, &fp, &qv, p.limit)
                .map_err(|e| rpc_err(-32004, e))?
        }
        None => db
            .vectorsearch(&p.duration, &query_json, p.limit)
            .map_err(|e| rpc_err(-32004, e))?,
    };
    let results: Vec<JsonValue> = hits.into_iter().map(|(id, ts, score)| serde_json::json!({
        "id":        id.to_string(),
        "timestamp": ts,
        "score":     score,
    })).collect();
    Ok(serde_json::json!({ "results": results }))
}
