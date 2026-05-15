use super::params::{pipe_err, rpc_err};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;

#[derive(serde::Deserialize)]
struct AddFileSyslogParams {
    #[allow(dead_code)]
    session: String,
    path: String,
    /// Optional explicit source override applied to every syslog
    /// record parsed from this file.  When `None`, the per-record
    /// resolution chain falls through to the parsed RFC 3164
    /// `host` field (auto-promoted from `data.host` via the
    /// `source_keys` chain), then to the deployment default.  The
    /// most common operator use: assign a logical pipeline name
    /// like `"syslog-shipper-a"` so every record from this file
    /// shares a source even when the hostnames vary.
    #[serde(default)]
    source: Option<String>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/add.file.syslog", |params, _ctx, _| async move {
            log::debug!("v2/add.file.syslog: start");
            let p: AddFileSyslogParams = params.parse()?;

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

            // Pipe wire format: `{path, source?}` object.  The
            // consumer (`bdsnode::server::add_file_syslog`) accepts
            // both the legacy bare-string shape and this object —
            // see that worker for the parse logic.
            let msg = serde_json::json!({ "path": p.path, "source": p.source });
            bdslib::pipe::send("ingest_file_syslog", msg).map_err(pipe_err)?;

            log::debug!("v2/add.file.syslog: queued {:?} (source={:?})", p.path, p.source);
            Ok::<serde_json::Value, ErrorObject>(serde_json::json!({
                "queued": p.path,
                "source": p.source,
            }))
        })
        .unwrap();
}
