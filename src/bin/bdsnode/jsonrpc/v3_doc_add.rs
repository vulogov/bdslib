//! `v3/doc.add` — fully-replicated document add.  Same shape as
//! `v2/doc.add` but the coordinator also fans out to **every** Alive peer
//! via `v2/doc.add` (with `id` injected for shared identity).

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
    content:  String,
    /// Optional caller-supplied UUIDv7 (preserved on retries).
    #[serde(default)] id: Option<String>,
}

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v3/doc.add", |params, _ctx, _| async move {
        log::debug!("v3/doc.add: start");
        let p: Params = params.parse()?;

        let id = match p.id.as_deref() {
            Some(s) => Uuid::parse_str(s).map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?,
            None    => Uuid::now_v7(),
        };

        // Stamp updated_at into metadata so anti-entropy can use it for LWW.
        let mut metadata = p.metadata;
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("updated_at".into(), JsonValue::from(now_secs()));
        }

        // Local write.
        let id_local = id;
        let meta_local = metadata.clone();
        let content_local = p.content.clone();
        tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            // Idempotent: if already present locally (replication retry),
            // skip the write and return the existing id.
            if db.doc_get_metadata(id_local).map_err(|e| rpc_err(-32011, e))?.is_some() {
                return Ok::<(), ErrorObject>(());
            }
            db.doc_add_with_id(id_local, meta_local, content_local.as_bytes())
                .map_err(|e| rpc_err(-32011, e))
        })
        .await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        // Fan-out to all Alive peers.
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let outcome = match &cluster {
            Some(c) => {
                let v2_params = serde_json::json!({
                    "id":       id.to_string(),
                    "metadata": metadata,
                    "content":  p.content,
                });
                Some(replicate_to_all(c.clone(), "v2/doc.add", v2_params).await)
            }
            None => None,
        };

        let outcome_json = outcome.as_ref().map(|o| o.to_json())
            .unwrap_or_else(|| serde_json::json!({"peers_attempted":0,"peers_succeeded":0,"hints_queued":0}));

        log::debug!("v3/doc.add: done id={id}");
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
