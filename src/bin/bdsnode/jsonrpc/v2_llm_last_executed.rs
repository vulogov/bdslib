//! `v2/llm.last_executed` — most-recent inference-log row for a
//! `cache_key` as seen by **this** node.
//!
//! Peer of `v2/scheduler.last_seen`.  The cluster-aware LLM helpers
//! fan this out to every Alive peer before invoking a provider on a
//! cache miss, taking the most-recent row to decide whether to skip
//! (someone else is running it / already ran it) or proceed.
//!
//! Returns `{found: false}` when this node has no record OR when
//! cluster mode is disabled (inference_log lives on Cluster, not on
//! standalone nodes — those can't dedup across the cluster anyway).

use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::{json, Value as JsonValue};

#[derive(serde::Deserialize)]
struct Params {
    cache_key: String,
    /// Optional: when set, restricts the lookup to rows whose
    /// `started_at >= now - window_secs`.  Mirrors the caller's
    /// dedup window so peers without the latest config don't return
    /// a stale row that the caller would have ignored anyway.
    #[serde(default)]
    window_secs: Option<u64>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/llm.last_executed", |params, _ctx, _| async move {
            let p: Params = params.parse()?;
            let resp = tokio::task::spawn_blocking(move || {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let cluster = match db.cluster() {
                    Some(c) => c,
                    None    => {
                        return Ok::<JsonValue, ErrorObject<'static>>(
                            json!({ "found": false })
                        );
                    }
                };
                let row = match p.window_secs {
                    Some(w) => cluster.inference_log.recent_within(&p.cache_key, w),
                    None    => cluster.inference_log.most_recent(&p.cache_key),
                }.map_err(|e| rpc_err(-32004, e))?;
                Ok(match row {
                    Some(r) => json!({
                        "found":       true,
                        "cache_key":   r.cache_key,
                        "started_at":  r.started_at,
                        "finished_at": r.finished_at,
                        "node_id":     r.node_id.to_string(),
                        "state":       r.state.as_str(),
                    }),
                    None => json!({ "found": false }),
                })
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            resp
        })
        .unwrap();
}
