use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;

#[derive(serde::Deserialize)]
struct AddBatchParams {
    docs: Vec<serde_json::Value>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/add.batch", |params, _ctx, _| async move {
            let p: AddBatchParams = params.parse()?;
            let n = p.docs.len();
            for doc in p.docs {
                bdslib::pipe::send("ingest", doc).map_err(|e| rpc_err(-32001, e))?;
            }
            Ok::<serde_json::Value, ErrorObject>(serde_json::json!({ "queued": n }))
        })
        .unwrap();
}
