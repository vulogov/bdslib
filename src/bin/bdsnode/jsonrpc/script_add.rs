use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct ScriptAddParams {
    #[allow(dead_code)]
    #[serde(default)]
    session: String,
    /// Metadata JSON object — must contain non-empty `name` and `schedule`.
    metadata: serde_json::Value,
    /// Raw BUND script body.
    script: String,
    /// Optional caller-supplied UUIDv7.  Used by `v3/script.add` fan-out
    /// so every replica writes the script under the same identity.
    #[serde(default)]
    id: Option<String>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/script_add", |params, _ctx, _| async move {
            log::debug!("v2/script_add: start");
            let p: ScriptAddParams = params.parse()?;
            let result = tokio::task::spawn_blocking(move || {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let id = match p.id.as_deref() {
                    Some(s) => {
                        let uuid = Uuid::parse_str(s)
                            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
                        if db.script_metadata(uuid).map_err(|e| rpc_err(-32011, e))?.is_some() {
                            return Ok::<serde_json::Value, ErrorObject>(
                                serde_json::json!({ "id": uuid.to_string(), "existing": true })
                            );
                        }
                        db.script_add_with_id(uuid, p.metadata, &p.script)
                            .map_err(|e| rpc_err(-32600, e))?;
                        uuid
                    }
                    None => db.script_add(p.metadata, &p.script)
                        .map_err(|e| rpc_err(-32600, e))?,
                };
                Ok::<serde_json::Value, ErrorObject>(serde_json::json!({ "id": id.to_string() }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            log::debug!("v2/script_add: done");
            result
        })
        .unwrap();
}
