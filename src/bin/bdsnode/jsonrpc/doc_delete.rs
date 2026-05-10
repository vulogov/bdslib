use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(serde::Deserialize)]
struct DocDeleteParams {
    #[allow(dead_code)]
    #[serde(default)]
    session: String,
    id: String,
    /// Optional deletion timestamp (Unix seconds).  Used by replicated
    /// `v3/doc.delete` so all replicas tombstone with the same timestamp.
    /// When absent, the receiver uses its local wall clock.
    #[serde(default)]
    deleted_at: Option<i64>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/doc.delete", |params, _ctx, _| async move {
            log::debug!("v2/doc.delete: start");
            let p: DocDeleteParams = params.parse()?;
            let id = uuid::Uuid::parse_str(&p.id)
                .map_err(|e| rpc_err(-32600, format!("invalid UUID {:?}: {e}", p.id)))?;
            let deleted_at = p.deleted_at.unwrap_or_else(now_secs);
            let result = tokio::task::spawn_blocking(move || {
                log::debug!("v2/doc.delete: id={id}");
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                db.doc_delete(id).map_err(|e| rpc_err(-32011, e))?;
                // Tombstone the deletion so anti-entropy doesn't resurrect
                // it from peers that haven't yet learned about it.
                if let Some(c) = db.cluster() {
                    if let Err(e) = c.tombstones.mark_deleted("docs", id, deleted_at) {
                        log::warn!("v2/doc.delete: tombstone {id}: {e}");
                    }
                }
                Ok::<serde_json::Value, ErrorObject>(serde_json::json!({
                    "deleted":    true,
                    "deleted_at": deleted_at,
                }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            log::debug!("v2/doc.delete: done");
            result
        })
        .unwrap();
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
