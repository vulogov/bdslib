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
use bdslib::cluster::fanout;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

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

#[derive(Clone)]
struct Hit {
    id:        String,
    timestamp: i64,
    score:     f64,
    n_seen:    u64,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v3/search", |params, _ctx, _| async move {
            log::debug!("v3/search: start");
            let p: SearchParams = params.parse()?;

            // Fan-out forwards exactly the same shape the local v2/search
            // accepts.  We pass the *full* params so peers' v2/search runs
            // identical scoring; merging happens here.
            let v2_params = serde_json::json!({
                "session":  p.session,
                "query":    p.query,
                "duration": p.duration,
                "limit":    p.limit,
            });

            let p_local = p.clone();
            let local_fut = tokio::task::spawn_blocking(move || local_search(&p_local));

            let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
            let fanout_fut = async {
                match &cluster {
                    Some(c) => Some(fanout::fan_out_v2(c, "v2/search", v2_params).await),
                    None    => None,
                }
            };

            let (local_res, fan) = tokio::join!(local_fut, fanout_fut);
            let local = local_res
                .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

            // Merge: HashMap<id, Hit>.  When the same UUID appears on
            // multiple peers, average the per-peer scores so replicated
            // records don't outrank single-replica records by accident.
            let mut by_id: HashMap<String, Hit> = HashMap::new();
            absorb_results(&mut by_id, &local);
            if let Some(f) = &fan {
                for r in f.ok_results() {
                    absorb_results(&mut by_id, r);
                }
            }

            let mut merged: Vec<Hit> = by_id.into_values().collect();
            // averaged score = score / n_seen
            for h in &mut merged {
                if h.n_seen > 1 {
                    h.score /= h.n_seen as f64;
                }
            }
            merged.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
            merged.truncate(p.limit);

            let results: Vec<JsonValue> = merged.into_iter().map(|h| serde_json::json!({
                "id":         h.id,
                "timestamp":  h.timestamp,
                "score":      h.score,
                "replicas":   h.n_seen,
            })).collect();

            log::debug!("v3/search: done (n={})", results.len());
            Ok::<JsonValue, ErrorObject>(serde_json::json!({
                "results":      results,
                "cluster_meta": v3_cluster_meta(fan),
            }))
        })
        .unwrap();
}

fn absorb_results(by_id: &mut HashMap<String, Hit>, body: &JsonValue) {
    let arr = match body.get("results").and_then(|v| v.as_array()) {
        Some(a) => a,
        None    => return,
    };
    for item in arr {
        let id  = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        if id.is_empty() { continue; }
        let ts  = item.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
        let sc  = item.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        match by_id.get_mut(&id) {
            Some(h) => {
                h.score  += sc;          // sum-then-divide gives the mean
                h.n_seen += 1;
            }
            None => {
                by_id.insert(id.clone(), Hit { id, timestamp: ts, score: sc, n_seen: 1 });
            }
        }
    }
}

fn local_search(p: &SearchParams) -> Result<JsonValue, ErrorObject<'static>> {
    let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
    let query_json = serde_json::json!(p.query);
    let hits = db
        .vectorsearch(&p.duration, &query_json, p.limit)
        .map_err(|e| rpc_err(-32004, e))?;
    let results: Vec<JsonValue> = hits.into_iter().map(|(id, ts, score)| serde_json::json!({
        "id":        id.to_string(),
        "timestamp": ts,
        "score":     score,
    })).collect();
    Ok(serde_json::json!({ "results": results }))
}
