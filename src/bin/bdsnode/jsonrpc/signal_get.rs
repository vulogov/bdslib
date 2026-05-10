//! `v2/signal.get` — fetch a single signal's metadata by UUID.
//!
//! Added in Phase 5 to give anti-entropy a way to pull a missing signal
//! by id (the original v2/signals API only exposed lookup by recency or
//! semantic query).

use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct Params {
    #[allow(dead_code)] #[serde(default)] session: String,
    id: String,
}

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v2/signal.get", |params, _ctx, _| async move {
        let p: Params = params.parse()?;
        let id = Uuid::parse_str(&p.id)
            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
        let result = tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let metadata = db.signal_get(id).map_err(|e| rpc_err(-32011, e))?;
            Ok::<serde_json::Value, ErrorObject>(serde_json::json!({
                "id":       id.to_string(),
                "metadata": metadata,
            }))
        })
        .await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
        result
    }).unwrap();
}
