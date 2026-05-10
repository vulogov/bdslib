//! `v3/script.add` — fully-replicated BUND script add.

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
    metadata: JsonValue,
    script:   String,
    #[serde(default)] id: Option<String>,
}

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v3/script.add", |params, _ctx, _| async move {
        log::debug!("v3/script.add: start");
        let p: Params = params.parse()?;

        let id = match p.id.as_deref() {
            Some(s) => Uuid::parse_str(s).map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?,
            None    => Uuid::now_v7(),
        };

        let mut metadata = p.metadata;
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("updated_at".into(), JsonValue::from(now_secs()));
        }

        let id_local = id;
        let meta_local = metadata.clone();
        let script_local = p.script.clone();
        tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            if db.script_metadata(id_local).map_err(|e| rpc_err(-32011, e))?.is_some() {
                return Ok::<(), ErrorObject>(());
            }
            db.script_add_with_id(id_local, meta_local, &script_local)
                .map_err(|e| rpc_err(-32600, e))
        })
        .await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let outcome = match &cluster {
            Some(c) => {
                let v2_params = serde_json::json!({
                    "id":       id.to_string(),
                    "metadata": metadata,
                    "script":   p.script,
                });
                Some(replicate_to_all(c.clone(), "v2/script_add", v2_params).await)
            }
            None => None,
        };

        let outcome_json = outcome.as_ref().map(|o| o.to_json())
            .unwrap_or_else(|| serde_json::json!({"peers_attempted":0,"peers_succeeded":0,"hints_queued":0}));

        log::debug!("v3/script.add: done id={id}");
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "id":           id.to_string(),
            "outcome":      outcome_json,
            "cluster_meta": v3_cluster_meta(None::<FanOutResults>),
        }))
    }).unwrap();
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
