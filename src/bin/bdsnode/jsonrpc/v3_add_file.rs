//! `v3/add.file` and `v3/add.file.syslog` — replicated file ingest.
//!
//! Unlike the v2 counterparts (which queue the *path* into the local
//! async ingest channel), the v3 variants are **synchronous**: the
//! coordinator reads + parses the file, then submits the parsed records
//! through the same replication path as `v3/add.batch`.  Each record is
//! sharded-replicated to RF-1 random Alive peers.
//!
//! Why synchronous?  We need each record to flow through `ingest_batch`
//! so it gets a UUIDv7 the coordinator can preserve for replication.
//! The async file-queue path doesn't expose UUIDs.
//!
//! For very large files chunk yourself client-side and call repeatedly.

use super::params::rpc_err;
use super::v3_add_batch::ingest_batch;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

#[derive(serde::Deserialize)]
struct Params {
    #[allow(dead_code)] #[serde(default)] session: String,
    path: String,
    #[serde(default)] replication_factor: Option<usize>,
}

pub fn register(module: &mut RpcModule<()>) {
    register_one(module, "v3/add.file",        Format::Ndjson);
    register_one(module, "v3/add.file.syslog", Format::Syslog);
}

#[derive(Copy, Clone)]
enum Format { Ndjson, Syslog }

fn register_one(module: &mut RpcModule<()>, method: &'static str, fmt: Format) {
    module.register_async_method(method, move |params, _ctx, _| async move {
        let p: Params = params.parse()?;

        let meta = std::fs::metadata(&p.path)
            .map_err(|e| rpc_err(-32600, format!("cannot access {:?}: {e}", p.path)))?;
        if !meta.is_file() {
            return Err(rpc_err(-32600, format!("{:?} is not a regular file", p.path)));
        }
        if meta.len() == 0 {
            return Err(rpc_err(-32600, format!("{:?} is empty", p.path)));
        }

        // Parse off the tokio runtime — file I/O is sync.
        let path = p.path.clone();
        let docs = tokio::task::spawn_blocking(move || -> Result<Vec<JsonValue>, ErrorObject<'static>> {
            let mut out: Vec<JsonValue> = Vec::new();
            match fmt {
                Format::Ndjson => {
                    let parse_json = |line: &str| -> bdslib::common::error::Result<JsonValue> {
                        serde_json::from_str(line)
                            .map_err(|e| bdslib::common::error::err_msg(format!("invalid JSON: {e}")))
                    };
                    bdslib::common::logparser::ingest_file(parse_json, |doc| out.push(doc), &path)
                        .map_err(|e| rpc_err(-32602, format!("parse failed: {e}")))?;
                }
                Format::Syslog => {
                    bdslib::common::logparser::ingest_file(
                        bdslib::common::logparser::parse_syslog,
                        |doc| out.push(doc),
                        &path,
                    ).map_err(|e| rpc_err(-32602, format!("parse failed: {e}")))?;
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        if docs.is_empty() {
            return Ok::<JsonValue, ErrorObject>(serde_json::json!({
                "path": p.path, "n": 0, "ids": [],
            }));
        }

        // Hand off to the same path v3/add.batch uses.
        let mut result = ingest_batch(docs, p.replication_factor).await?;
        if let Some(obj) = result.as_object_mut() {
            obj.insert("path".into(), JsonValue::String(p.path));
        }
        Ok(result)
    }).unwrap();
}
