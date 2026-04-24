use super::params::{rpc_err, TimeWindow, TimeWindowParams};
use jsonrpsee::RpcModule;
use std::collections::BTreeSet;

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/keys", |params, _ctx, _| async move {
            let p: TimeWindowParams = params.parse().unwrap_or_default();
            let window = p.resolve()?;

            tokio::task::spawn_blocking(move || {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let cache = db.cache();

                let shard_infos = match &window {
                    TimeWindow::All => cache.info().list_all(),
                    TimeWindow::Range(s, e) => cache.info().shards_in_range(*s, *e),
                }
                .map_err(|e| rpc_err(-32002, e))?;

                let mut keys: BTreeSet<String> = BTreeSet::new();
                for si in shard_infos {
                    let shard = cache.shard(si.start_time).map_err(|e| rpc_err(-32003, e))?;
                    let obs = shard.observability();
                    let shard_keys = match &window {
                        TimeWindow::All => obs.list_primary_keys_all(),
                        TimeWindow::Range(s, e) => obs.list_primary_keys_in_range(*s, *e),
                    }
                    .map_err(|e| rpc_err(-32004, e))?;
                    keys.extend(shard_keys);
                }

                let keys: Vec<String> = keys.into_iter().collect();
                Ok::<serde_json::Value, jsonrpsee::types::ErrorObject>(
                    serde_json::json!({ "keys": keys }),
                )
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?
        })
        .unwrap();
}
