use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct DocAddParams {
    #[allow(dead_code)]
    #[serde(default)]
    session: String,
    metadata: serde_json::Value,
    content: String,
    /// Optional caller-supplied UUIDv7.  Used by `v3/doc.add` fan-out so
    /// every replica writes the document under the same identity.  When
    /// absent, a fresh UUIDv7 is generated.
    #[serde(default)]
    id: Option<String>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/doc.add", |params, _ctx, _| async move {
            log::debug!("v2/doc.add: start");
            let p: DocAddParams = params.parse()?;
            let result = tokio::task::spawn_blocking(move || {
                log::debug!("v2/doc.add: session={}", p.session);
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;

                let id = match p.id.as_deref() {
                    Some(s) => {
                        let uuid = Uuid::parse_str(s)
                            .map_err(|e| rpc_err(-32602, format!("invalid id: {e}")))?;
                        // Idempotent receiver — anti-entropy / replication
                        // retries can re-arrive at the same UUID.  When the
                        // doc already exists, return its id without re-writing.
                        if db.doc_get_metadata(uuid).map_err(|e| rpc_err(-32011, e))?.is_some() {
                            return Ok::<serde_json::Value, ErrorObject>(
                                serde_json::json!({ "id": uuid.to_string(), "existing": true })
                            );
                        }
                        db.doc_add_with_id(uuid, p.metadata, p.content.as_bytes())
                            .map_err(|e| rpc_err(-32011, e))?;
                        uuid
                    }
                    None => db.doc_add(p.metadata, p.content.as_bytes())
                        .map_err(|e| rpc_err(-32011, e))?,
                };
                Ok::<serde_json::Value, ErrorObject>(serde_json::json!({ "id": id.to_string() }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            log::debug!("v2/doc.add: done");
            result
        })
        .unwrap();
}
