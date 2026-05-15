use super::params::{pipe_err, rpc_err};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;

#[derive(serde::Deserialize)]
struct AddFileParams {
    #[allow(dead_code)]
    session: String,
    path: String,
    /// Optional explicit source override applied to every record
    /// parsed from this file.  When `None`, per-record resolution
    /// runs as usual (typically lands the deployment default, since
    /// NDJSON records rarely include a host/origin tag on their own).
    #[serde(default)]
    source: Option<String>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/add.file", |params, _ctx, _| async move {
            log::debug!("v2/add.file: start");
            let p: AddFileParams = params.parse()?;

            let meta = std::fs::metadata(&p.path).map_err(|e| {
                rpc_err(-32600, format!("cannot access {:?}: {e}", p.path))
            })?;

            if !meta.is_file() {
                return Err(rpc_err(-32600, format!("{:?} is not a regular file", p.path)));
            }
            if meta.len() == 0 {
                return Err(rpc_err(-32600, format!("{:?} is empty", p.path)));
            }

            // Verify read access by opening the file.
            std::fs::File::open(&p.path).map_err(|e| {
                rpc_err(-32600, format!("cannot open {:?}: {e}", p.path))
            })?;

            // Pipe wire format: `{path, source?}` object.  A bare-string
            // value is still accepted on the consumer side for
            // backwards compatibility with anything that bypasses this
            // handler.
            let msg = serde_json::json!({ "path": p.path, "source": p.source });
            bdslib::pipe::send("ingest_file", msg).map_err(pipe_err)?;

            log::debug!("v2/add.file: queued {:?} (source={:?})", p.path, p.source);
            Ok::<serde_json::Value, ErrorObject>(serde_json::json!({
                "queued": p.path,
                "source": p.source,
            }))
        })
        .unwrap();
}
