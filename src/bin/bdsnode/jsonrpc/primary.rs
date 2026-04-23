use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct PrimaryParams {
    primary_id: String,
}

/// Try the fast path first (UUID timestamp → single shard), then fall back to
/// a linear scan across all shards.  The fast path works for records ingested
/// after the `generate_v7_at` fix; the fallback covers older records whose UUID
/// embeds the wall-clock ingest time rather than the event timestamp.
fn find_in_shards(
    uuid: Uuid,
    db: &bdslib::ShardsManager,
) -> Result<(serde_json::Value, usize), ErrorObject<'static>> {
    let cache = db.cache();
    let info = cache.info();

    // ── fast path: shard derived from UUID timestamp ──────────────────────────
    if let Some(system_time) = bdslib::timestamp_from_v7(uuid) {
        if let Ok(shard_infos) = info.shards_at(system_time) {
            for si in shard_infos {
                if let Ok(shard) = cache.shard(si.start_time) {
                    let obs = shard.observability();
                    if let Ok(Some(doc)) = obs.get_by_id(uuid) {
                        let count = obs.list_secondaries(uuid).map(|v| v.len()).unwrap_or(0);
                        return Ok((doc, count));
                    }
                }
            }
        }
    }

    // ── fallback: scan all shards ─────────────────────────────────────────────
    let all_shards = info.list_all().map_err(|e| rpc_err(-32002, e))?;
    for si in all_shards {
        let shard = cache.shard(si.start_time).map_err(|e| rpc_err(-32003, e))?;
        let obs = shard.observability();
        if let Ok(Some(doc)) = obs.get_by_id(uuid) {
            let count = obs.list_secondaries(uuid).map(|v| v.len()).unwrap_or(0);
            return Ok((doc, count));
        }
    }

    Err(rpc_err(-32404, format!("primary {uuid} not found")))
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/primary", |params, _ctx, _| async move {
            let p: PrimaryParams = params.parse()?;

            tokio::task::spawn_blocking(move || {
                let uuid = Uuid::parse_str(&p.primary_id)
                    .map_err(|e| rpc_err(-32600, format!("invalid UUID {:?}: {e}", p.primary_id)))?;

                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;

                let (mut doc, secondaries_count) = find_in_shards(uuid, db)?;

                if let Some(obj) = doc.as_object_mut() {
                    obj.insert("secondaries_count".to_string(), serde_json::json!(secondaries_count));
                }

                Ok::<serde_json::Value, ErrorObject>(doc)
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?
        })
        .unwrap();
}
