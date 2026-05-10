use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;

#[derive(serde::Deserialize)]
struct DocUpdateMetadataParams {
    #[allow(dead_code)]
    #[serde(default)]
    session: String,
    id: String,
    metadata: serde_json::Value,
    /// LWW guard: when true, the receiver only applies the update if the
    /// incoming `metadata.updated_at` is strictly greater than the
    /// currently-stored value.  Used by anti-entropy and (via Phase 5
    /// updates) by `v3/doc.update.metadata` fan-out so concurrent partition
    /// edits don't overwrite each other based on arrival order.
    #[serde(default)]
    if_newer: bool,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/doc.update.metadata", |params, _ctx, _| async move {
            log::debug!("v2/doc.update.metadata: start");
            let p: DocUpdateMetadataParams = params.parse()?;
            let id = uuid::Uuid::parse_str(&p.id)
                .map_err(|e| rpc_err(-32600, format!("invalid UUID {:?}: {e}", p.id)))?;
            let result = tokio::task::spawn_blocking(move || {
                log::debug!("v2/doc.update.metadata: id={id} if_newer={}", p.if_newer);
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;

                if p.if_newer {
                    let existing = db.doc_get_metadata(id).map_err(|e| rpc_err(-32011, e))?;
                    let local_ts = existing.as_ref()
                        .and_then(|m| m.get("updated_at"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let remote_ts = p.metadata.get("updated_at")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if remote_ts <= local_ts {
                        return Ok::<serde_json::Value, ErrorObject>(serde_json::json!({
                            "updated":   false,
                            "reason":    "stale",
                            "local_at":  local_ts,
                            "remote_at": remote_ts,
                        }));
                    }
                }

                db.doc_update_metadata(id, p.metadata)
                    .map_err(|e| rpc_err(-32011, e))?;
                Ok::<serde_json::Value, ErrorObject>(serde_json::json!({ "updated": true }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            log::debug!("v2/doc.update.metadata: done");
            result
        })
        .unwrap();
}
