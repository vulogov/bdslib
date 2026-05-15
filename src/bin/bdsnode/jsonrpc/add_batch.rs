use super::params::{pipe_err, rpc_err};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;

#[derive(serde::Deserialize)]
struct AddBatchParams {
    docs: Vec<serde_json::Value>,
    /// When `true`, bypass the ingest queue and call
    /// `ShardsManager::add_batch` directly.  The response carries the
    /// assigned UUIDv7s instead of the bare queue acknowledgement.
    /// Default `false` (queued, fire-and-forget — historical behaviour).
    /// Used by `v3/add.batch` fan-out so the coordinator's hinted-handoff
    /// path can distinguish success from failure on the receiver.
    #[serde(default)]
    sync: bool,
    /// Optional explicit source override applied to EVERY doc in the
    /// batch.  When `None`, each doc resolves independently — a mixed
    /// batch with per-record `host` fields will get per-record sources.
    #[serde(default)]
    source: Option<String>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/add.batch", |params, _ctx, _| async move {
            log::debug!("v2/add.batch: start");
            let p: AddBatchParams = params.parse()?;
            let n = p.docs.len();

            if p.sync {
                let source = p.source.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                    let ids = db
                        .add_batch_with_source(p.docs, source.as_deref())
                        .map_err(|e| rpc_err(-32004, e))?;
                    let arr: Vec<serde_json::Value> = ids.into_iter()
                        .map(|u| serde_json::json!(u.to_string()))
                        .collect();
                    Ok::<serde_json::Value, ErrorObject>(serde_json::json!({
                        "ids":    arr,
                        "n":      n,
                        "synced": true,
                    }))
                })
                .await
                .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
                log::debug!("v2/add.batch: done (sync, n={n})");
                return result;
            }

            // Queued path — resolve + inject source on every doc
            // before bulk-pushing, so the flusher sees pre-tagged
            // records.  Same idempotent re-resolution dance as
            // `v2/add` queued path.
            let cfg = bdslib::common::source::get();
            let mut docs = p.docs;
            for d in docs.iter_mut() {
                let resolved = bdslib::common::source::resolve(p.source.as_deref(), d, cfg);
                bdslib::common::source::inject(d, &resolved);
            }
            // Bulk-push all docs in a single helper call: the channel mutex
            // is taken once per item (instead of once per call site) and the
            // tokio worker is freed up sooner.
            bdslib::pipe::send_many("ingest", docs).map_err(pipe_err)?;
            log::debug!("v2/add.batch: done (queued, n={n})");
            Ok::<serde_json::Value, ErrorObject>(serde_json::json!({ "queued": n }))
        })
        .unwrap();
}
