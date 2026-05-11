//! `v3/signals` (recent) and `v3/signals_query` (semantic search) — cluster-wide.
//!
//! Signals are fully replicated by Phase 4 anti-entropy, so in steady state
//! every node has every signal.  We still fan out so partial replication
//! states (e.g. mid-recovery) heal in the response.
//!
//! - `v3/signals`: UUID dedup with first-seen wins.
//! - `v3/signals_query`: UUID dedup + score average (like v3/search).

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::{fanout, merge};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

pub fn register(module: &mut RpcModule<()>) {
    register_signals(module);
    register_signals_query(module);
}

fn register_signals(module: &mut RpcModule<()>) {
    module.register_async_method("v3/signals", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
        let dur = raw.get("duration").and_then(|v| v.as_str()).unwrap_or("1h").to_owned();

        let dur_local = dur.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let ids = db.signals_recent(&dur_local).map_err(|e| rpc_err(-32011, e))?;
            let mut signals: Vec<JsonValue> = Vec::with_capacity(ids.len());
            for id_str in &ids {
                if let Ok(uuid) = uuid::Uuid::parse_str(id_str) {
                    if let Ok(Some(meta)) = db.signal_get(uuid) {
                        signals.push(serde_json::json!({"id": id_str, "metadata": meta}));
                        continue;
                    }
                }
                signals.push(serde_json::json!({"id": id_str, "metadata": null}));
            }
            Ok(serde_json::json!({"duration": dur_local, "count": signals.len(), "signals": signals}))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/signals", raw).await),
            None    => None,
        };
        let bodies = merge::bodies_from(&local, fan.as_ref());
        let signals = merge::dedup_by_id(bodies, "signals");

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "duration":     dur,
            "count":        signals.len(),
            "signals":      signals,
            "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}

fn register_signals_query(module: &mut RpcModule<()>) {
    module.register_async_method("v3/signals_query", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
        let query = raw.get("query").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'query'"))?.to_owned();
        let limit = raw.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

        let q_local = query.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let results = db.signals_query(&q_local, limit).map_err(|e| rpc_err(-32011, e))?;
            Ok(serde_json::json!({"query": q_local, "count": results.len(), "results": results}))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/signals_query", raw).await),
            None    => None,
        };
        let bodies = merge::bodies_from(&local, fan.as_ref());
        let mut results = merge::dedup_avg_score(bodies, "results");
        results.truncate(limit);

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "query":        query,
            "count":        results.len(),
            "results":      results,
            "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}
