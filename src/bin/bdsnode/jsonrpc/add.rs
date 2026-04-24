use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;

#[derive(serde::Deserialize)]
struct AddParams {
    doc: serde_json::Value,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/add", |params, _ctx, _| async move {
            let p: AddParams = params.parse()?;
            bdslib::pipe::send("ingest", p.doc).map_err(|e| rpc_err(-32001, e))?;
            Ok::<serde_json::Value, ErrorObject>(serde_json::json!({ "queued": 1 }))
        })
        .unwrap();
}
