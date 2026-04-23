use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn err(code: i32, msg: impl std::fmt::Display) -> ErrorObject<'static> {
    ErrorObject::owned(code, msg.to_string(), None::<()>)
}

#[derive(serde::Deserialize, Default)]
struct CountParams {
    /// Lookback window, e.g. `"1h"`, `"30min"`. Mutually exclusive with
    /// `start_ts` / `end_ts`.
    duration: Option<String>,
    /// Range start as Unix seconds. Requires `end_ts`.
    start_ts: Option<i64>,
    /// Range end as Unix seconds. Requires `start_ts`.
    end_ts: Option<i64>,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/count", |params, _ctx, _| async move {
            let p: CountParams = params.parse().unwrap_or_default();

            tokio::task::spawn_blocking(move || {
                let db = bdslib::get_db().map_err(|e| err(-32001, e))?;
                let cache = db.cache();
                let info = cache.info();

                // ── resolve time window ───────────────────────────────────────
                enum Window {
                    All,
                    Range(SystemTime, SystemTime),
                }

                let window = if let Some(ref d) = p.duration {
                    let secs = humantime::parse_duration(d)
                        .map_err(|e| err(-32600, format!("invalid duration {d:?}: {e}")))?
                        .as_secs();
                    let end = SystemTime::now();
                    let start = end - Duration::from_secs(secs);
                    Window::Range(start, end)
                } else if let (Some(s), Some(e)) = (p.start_ts, p.end_ts) {
                    let start = UNIX_EPOCH + Duration::from_secs(s as u64);
                    let end = UNIX_EPOCH + Duration::from_secs(e as u64);
                    Window::Range(start, end)
                } else {
                    Window::All
                };

                // ── select shards ─────────────────────────────────────────────
                let shard_infos = match &window {
                    Window::All => info.list_all().map_err(|e| err(-32002, e))?,
                    Window::Range(s, e) => {
                        info.shards_in_range(*s, *e).map_err(|e| err(-32002, e))?
                    }
                };

                // ── sum counts across shards ──────────────────────────────────
                let mut total: u64 = 0;
                for si in shard_infos {
                    let shard = cache.shard(si.start_time).map_err(|e| err(-32003, e))?;
                    let obs = shard.observability();
                    let n = match &window {
                        Window::All => obs.count_all(),
                        Window::Range(s, e) => obs.count_in_range(*s, *e),
                    }
                    .map_err(|e| err(-32004, e))?;
                    total += n;
                }

                Ok::<serde_json::Value, ErrorObject>(serde_json::json!({ "count": total }))
            })
            .await
            .map_err(|e| err(-32000, format!("task panicked: {e}")))?
        })
        .unwrap();
}
