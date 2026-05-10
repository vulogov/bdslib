use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct ScriptDeleteParams {
    #[allow(dead_code)]
    #[serde(default)]
    session: String,
    /// UUIDv7 string of the script to delete.
    id: String,
    /// Optional deletion timestamp; see `v2/doc.delete` for semantics.
    #[serde(default)]
    deleted_at: Option<i64>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/script_delete", |params, _ctx, _| async move {
            log::debug!("v2/script_delete: start");
            let p: ScriptDeleteParams = params.parse()?;
            let deleted_at = p.deleted_at.unwrap_or_else(now_secs);
            let result = tokio::task::spawn_blocking(move || {
                let id = Uuid::parse_str(&p.id)
                    .map_err(|e| rpc_err(-32600, format!("invalid id {:?}: {e}", p.id)))?;
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                db.script_delete(id).map_err(|e| rpc_err(-32004, e))?;
                if let Some(c) = db.cluster() {
                    if let Err(e) = c.tombstones.mark_deleted("scripts", id, deleted_at) {
                        log::warn!("v2/script_delete: tombstone {id}: {e}");
                    }
                }
                Ok::<serde_json::Value, ErrorObject>(serde_json::json!({
                    "id":         p.id,
                    "deleted":    true,
                    "deleted_at": deleted_at,
                }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            log::debug!("v2/script_delete: done");
            result
        })
        .unwrap();
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
