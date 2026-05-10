//! `v3/doc.update.content` — fully-replicated document content update.
//!
//! Same fan-out semantics as `v3/doc.update.metadata`.  Note: this only
//! updates the content blob; the metadata's `updated_at` is **not**
//! bumped automatically (no metadata write is involved).  If you need
//! anti-entropy to surface this update later, follow up with
//! `v3/doc.update.metadata` to bump `updated_at` on the metadata side.

use super::params::{rpc_err, v3_cluster_meta};
use super::v3_replicated::replicate_to_all;
use bdslib::cluster::fanout::FanOutResults;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct Params {
    #[serde(default)] #[allow(dead_code)] session: String,
    id: String,
    content: String,
}

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v3/doc.update.content", |params, _ctx, _| async move {
        log::debug!("v3/doc.update.content: start");
        let p: Params = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;

        let content_local = p.content.clone();
        tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            db.doc_update_content(id, content_local.as_bytes())
                .map_err(|e| rpc_err(-32011, e))
        })
        .await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let outcome = match &cluster {
            Some(c) => {
                let v2_params = serde_json::json!({
                    "id":      id.to_string(),
                    "content": p.content,
                });
                Some(replicate_to_all(c.clone(), "v2/doc.update.content", v2_params).await)
            }
            None => None,
        };

        let outcome_json = outcome.as_ref().map(|o| o.to_json())
            .unwrap_or_else(|| serde_json::json!({"peers_attempted":0,"peers_succeeded":0,"hints_queued":0}));

        log::debug!("v3/doc.update.content: done id={id}");
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "id":           id.to_string(),
            "updated":      true,
            "outcome":      outcome_json,
            "cluster_meta": v3_cluster_meta(None::<FanOutResults>),
        }))
    }).unwrap();
}
