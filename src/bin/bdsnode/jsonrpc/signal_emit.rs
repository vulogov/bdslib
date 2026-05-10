use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct SignalEmitParams {
    #[allow(dead_code)]
    #[serde(default)]
    session:   String,
    name:      String,
    severity:  String,
    timestamp: u64,
    /// Optional extra fields merged into the stored metadata.
    /// `name`, `severity`, and `timestamp` always take precedence.
    #[serde(default)]
    metadata:  serde_json::Map<String, serde_json::Value>,
    /// Optional caller-supplied UUIDv7.  Used by `v3/signal.emit` fan-out
    /// so every replica writes the signal under the same identity.
    #[serde(default)]
    id: Option<String>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/signal.emit", |params, _ctx, _| async move {
            log::debug!("v2/signal.emit: start");
            let p: SignalEmitParams = params.parse()?;
            let result = tokio::task::spawn_blocking(move || {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let id = match p.id.as_deref() {
                    Some(s) => {
                        let uuid = Uuid::parse_str(s)
                            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
                        if db.signal_get(uuid).map_err(|e| rpc_err(-32011, e))?.is_some() {
                            return Ok::<serde_json::Value, ErrorObject>(
                                serde_json::json!({ "id": uuid.to_string(), "existing": true })
                            );
                        }
                        db.signal_emit_with_id(uuid, &p.name, &p.severity, p.timestamp, p.metadata)
                            .map_err(|e| rpc_err(-32011, e))?;
                        uuid
                    }
                    None => db.signal_emit(&p.name, &p.severity, p.timestamp, p.metadata)
                        .map_err(|e| rpc_err(-32011, e))?,
                };
                Ok::<serde_json::Value, ErrorObject>(serde_json::json!({ "id": id.to_string() }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            log::debug!("v2/signal.emit: done");
            result
        })
        .unwrap();
}
