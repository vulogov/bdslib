use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;

fn err(code: i32, msg: impl std::fmt::Display) -> ErrorObject<'static> {
    ErrorObject::owned(code, msg.to_string(), None::<()>)
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/timeline", |_params, _ctx, _| async move {
            tokio::task::spawn_blocking(|| {
                let db = bdslib::get_db().map_err(|e| err(-32001, e))?;

                let shards = db
                    .cache()
                    .info()
                    .list_all()
                    .map_err(|e| err(-32002, e))?;

                let mut global_min: Option<i64> = None;
                let mut global_max: Option<i64> = None;

                for info in shards {
                    let shard = db
                        .cache()
                        .shard(info.start_time)
                        .map_err(|e| err(-32003, e))?;

                    let (smin, smax) = shard
                        .observability()
                        .timestamp_range()
                        .map_err(|e| err(-32004, e))?;

                    if let Some(v) = smin {
                        global_min = Some(global_min.map_or(v, |cur| cur.min(v)));
                    }
                    if let Some(v) = smax {
                        global_max = Some(global_max.map_or(v, |cur| cur.max(v)));
                    }
                }

                Ok::<serde_json::Value, ErrorObject>(serde_json::json!({
                    "min_ts": global_min,
                    "max_ts": global_max,
                }))
            })
            .await
            .map_err(|e| err(-32000, format!("task panicked: {e}")))?
        })
        .unwrap();
}
