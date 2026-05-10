//! `v3/script.delete` — fully-replicated script delete.

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
}

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v3/script.delete", |params, _ctx, _| async move {
        log::debug!("v3/script.delete: start");
        let p: Params = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let deleted_at = now_secs();

        tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.script_delete(id).map_err(|e| rpc_err(-32004, e))?;
            if let Some(c) = db.cluster() {
                if let Err(e) = c.tombstones.mark_deleted("scripts", id, deleted_at) {
                    log::warn!("v3/script.delete: tombstone {id}: {e}");
                }
            }
            Ok::<(), ErrorObject>(())
        })
        .await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let outcome = match &cluster {
            Some(c) => {
                let v2_params = serde_json::json!({
                    "id":         id.to_string(),
                    "deleted_at": deleted_at,
                });
                Some(replicate_to_all(c.clone(), "v2/script_delete", v2_params).await)
            }
            None => None,
        };

        let outcome_json = outcome.as_ref().map(|o| o.to_json())
            .unwrap_or_else(|| serde_json::json!({"peers_attempted":0,"peers_succeeded":0,"hints_queued":0}));

        log::debug!("v3/script.delete: done id={id}");
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "id":           id.to_string(),
            "deleted":      true,
            "deleted_at":   deleted_at,
            "outcome":      outcome_json,
            "cluster_meta": v3_cluster_meta(None::<FanOutResults>),
        }))
    }).unwrap();
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
