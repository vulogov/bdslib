//! `v2/{doc,signal,script}.list_ids` — cheap UUID + updated_at + tombstone
//! enumeration used by the anti-entropy pull-sync.
//!
//! Live entries come from each store's `list_metadata()` walk; we extract
//! `updated_at` from the metadata (or fall back to `timestamp`, then `0`).
//! Tombstones come from the cluster's `TombstoneStorage` filtered by
//! store name.  When cluster mode is disabled, the `tombstones` array is
//! always empty.

use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

pub fn register(module: &mut RpcModule<()>) {
    register_for(module, "v2/doc.list_ids",    "docs",    StoreKind::Docs);
    register_for(module, "v2/signal.list_ids", "signals", StoreKind::Signals);
    register_for(module, "v2/script.list_ids", "scripts", StoreKind::Scripts);
}

#[derive(Copy, Clone)]
enum StoreKind { Docs, Signals, Scripts }

fn register_for(module: &mut RpcModule<()>, method: &'static str, store_name: &'static str, kind: StoreKind) {
    module.register_async_method(method, move |_params, _ctx, _| async move {
        log::debug!("{method}: start");
        let result = tokio::task::spawn_blocking(move || {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let entries = match kind {
                StoreKind::Docs    => db.docstore_list_metadata(),
                StoreKind::Signals => db.signals_list_metadata(),
                StoreKind::Scripts => db.scripts_with_metadata(),
            }.map_err(|e| rpc_err(-32004, e))?;

            let live: Vec<JsonValue> = entries.into_iter().map(|(id, meta)| {
                let updated_at = meta.get("updated_at").and_then(|v| v.as_u64())
                    .or_else(|| meta.get("timestamp").and_then(|v| v.as_u64()))
                    .unwrap_or(0);
                serde_json::json!({
                    "id":         id.to_string(),
                    "updated_at": updated_at,
                })
            }).collect();

            // Tombstones — empty when cluster mode is off.
            let tombstones: Vec<JsonValue> = match db.cluster() {
                Some(c) => c.tombstones.list_for_store(store_name)
                    .map_err(|e| rpc_err(-32004, e))?
                    .into_iter()
                    .map(|t| serde_json::json!({
                        "id":         t.id.to_string(),
                        "deleted_at": t.deleted_at,
                    }))
                    .collect(),
                None => Vec::new(),
            };

            Ok::<JsonValue, ErrorObject>(serde_json::json!({
                "store":      store_name,
                "n_live":     live.len(),
                "n_tombstones": tombstones.len(),
                "live":       live,
                "tombstones": tombstones,
            }))
        })
        .await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
        log::debug!("{method}: done");
        result
    }).unwrap();
}
