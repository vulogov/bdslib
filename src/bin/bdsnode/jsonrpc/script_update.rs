use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct ScriptUpdateParams {
    #[allow(dead_code)]
    #[serde(default)]
    session: String,
    /// UUIDv7 string of the script to update.
    id: String,
    /// New metadata — must contain non-empty `name` and `schedule`.
    metadata: serde_json::Value,
    /// New BUND script body.
    script: String,
    /// LWW guard: see `v2/doc.update.metadata` for semantics.
    #[serde(default)]
    if_newer: bool,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/script_update", |params, _ctx, _| async move {
            log::debug!("v2/script_update: start");
            let p: ScriptUpdateParams = params.parse()?;
            let result = tokio::task::spawn_blocking(move || {
                let id = Uuid::parse_str(&p.id)
                    .map_err(|e| rpc_err(-32600, format!("invalid id {:?}: {e}", p.id)))?;
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;

                if p.if_newer {
                    let existing = db.script_metadata(id).map_err(|e| rpc_err(-32011, e))?;
                    let local_ts = existing.as_ref()
                        .and_then(|m| m.get("updated_at"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let remote_ts = p.metadata.get("updated_at")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if remote_ts <= local_ts {
                        return Ok::<serde_json::Value, ErrorObject>(serde_json::json!({
                            "id":        p.id,
                            "updated":   false,
                            "reason":    "stale",
                            "local_at":  local_ts,
                            "remote_at": remote_ts,
                        }));
                    }
                }

                db.update_script(id, p.metadata, &p.script)
                    .map_err(|e| rpc_err(-32600, e))?;
                Ok::<serde_json::Value, ErrorObject>(serde_json::json!({ "id": p.id, "updated": true }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            log::debug!("v2/script_update: done");
            result
        })
        .unwrap();
}
