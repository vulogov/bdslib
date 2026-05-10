//! `v3/doc.update.metadata` — fully-replicated document metadata update.
//!
//! Bumps `metadata.updated_at` to wall-clock now, writes locally, then
//! fans out `v2/doc.update.metadata` to every Alive peer.  Real-time
//! semantics are last-real-time-update-wins (no LWW check at the
//! receiver).  Anti-entropy does not currently re-replicate updates —
//! see [CLUSTER.md § 11](../../CLUSTER.md#11-whats-not-yet-implemented)
//! for the documented Phase-4 limitation.

use super::params::{rpc_err, v3_cluster_meta};
use super::v3_replicated::replicate_to_all;
use bdslib::cluster::fanout::FanOutResults;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct Params {
    #[serde(default)] #[allow(dead_code)] session: String,
    id: String,
    metadata: JsonValue,
}

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v3/doc.update.metadata", |params, _ctx, _| async move {
        log::debug!("v3/doc.update.metadata: start");
        let p: Params = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;

        let mut metadata = p.metadata;
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("updated_at".into(), JsonValue::from(now_secs()));
        }

        let meta_local = metadata.clone();
        tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.doc_update_metadata(id, meta_local).map_err(|e| rpc_err(-32011, e))
        })
        .await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let outcome = match &cluster {
            Some(c) => {
                let v2_params = serde_json::json!({
                    "id":       id.to_string(),
                    "metadata": metadata,
                    "if_newer": true,    // LWW guard at the receiver
                });
                Some(replicate_to_all(c.clone(), "v2/doc.update.metadata", v2_params).await)
            }
            None => None,
        };

        let outcome_json = outcome.as_ref().map(|o| o.to_json())
            .unwrap_or_else(|| serde_json::json!({"peers_attempted":0,"peers_succeeded":0,"hints_queued":0}));

        log::debug!("v3/doc.update.metadata: done id={id}");
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "id":           id.to_string(),
            "updated":      true,
            "outcome":      outcome_json,
            "cluster_meta": v3_cluster_meta(None::<FanOutResults>),
        }))
    }).unwrap();
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
