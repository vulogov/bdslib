//! Primary / secondary record enumeration and fetch.
//!
//! | Helper                          | Cluster strategy                                        |
//! |---------------------------------|---------------------------------------------------------|
//! | `primaries`                     | sorted union of UUID strings                            |
//! | `primaries_explore`             | per-key sum count + UUID-set union (`merge_explore_rows`)|
//! | `primaries_explore_telemetry`   | same                                                    |
//! | `primaries_get`                 | UUID dedup + ts-DESC sort                               |
//! | `primaries_get_telemetry`       | same                                                    |
//! | `secondaries`                   | local-only (record lives on a single node only)         |
//! | `primary` / `secondary`         | local-only (DB is asked for this exact UUID)            |

use crate::cluster::merge;
use crate::vm::api::dispatch;
use crate::vm::api::time_window::{resolve_window, Window};
use crate::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use easy_error::{err_msg, Error};
use rust_dynamic::value::Value;
use serde_json::json;
use uuid::Uuid;

/// Distinct primary UUIDs across the (optional) time window.  In
/// cluster mode: sorted union of every peer's `ids[]` plus local.
///
/// `opts` may be `Value::nodata` (every shard) or a Map with
/// `duration` / `start_ts`+`end_ts`.
pub fn primaries(opts: Value) -> Result<Value, Error> {
    let opts_json = dynamic_to_json(opts);
    let merged = dispatch::read(
        "v2/primaries",
        opts_json.clone(),
        move || {
            let window = resolve_window(&opts_json)?;
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::primaries: get_db: {e}")))?;
            let cache = db.cache();
            let infos = window.list_shards(cache.info())
                .map_err(|e| err_msg(format!("vm::api::primaries: shards: {e}")))?;
            let mut ids: Vec<String> = Vec::new();
            for si in infos {
                let s = cache.shard(si.start_time)
                    .map_err(|e| err_msg(format!("vm::api::primaries: shard: {e}")))?;
                let obs = s.observability();
                let uuids = match window {
                    Window::All              => obs.list_primaries(),
                    Window::Range(start, end) => obs.list_primaries_in_range(start, end),
                }.map_err(|e| err_msg(format!("vm::api::primaries: list: {e}")))?;
                ids.extend(uuids.into_iter().map(|u| u.to_string()));
            }
            Ok(json!({"ids": ids}))
        },
        |local, fan| {
            let ids = merge::union_string_ids(&local, fan, "ids");
            json!({"ids": ids})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// `primaries.explore` — per-key UUID rollup.  Cluster merge sums
/// counts and unions UUID sets via `merge::merge_explore_rows`.
pub fn primaries_explore(duration: &str) -> Result<Value, Error> {
    explore_inner("v2/primaries.explore", duration, /* telemetry = */ false)
}

/// `primaries.explore.telemetry` — same shape but only over telemetry-
/// schema records.
pub fn primaries_explore_telemetry(duration: &str) -> Result<Value, Error> {
    explore_inner("v2/primaries.explore.telemetry", duration, /* telemetry = */ true)
}

fn explore_inner(method: &'static str, duration: &str, telemetry: bool) -> Result<Value, Error> {
    let merged = dispatch::read(
        method,
        json!({"session": "", "duration": duration}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::primaries_explore: get_db: {e}")))?;
            let entries = if telemetry {
                db.primaries_explore_telemetry(duration)
            } else {
                db.primaries_explore(duration)
            }.map_err(|e| err_msg(format!("vm::api::primaries_explore: db: {e}")))?;
            let arr: Vec<serde_json::Value> = entries.into_iter().map(|(key, count, ids)| {
                json!({
                    "key": key,
                    "count": count,
                    "primary_id": ids.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                })
            }).collect();
            Ok(serde_json::Value::Array(arr))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            json!({"results": merge::merge_explore_rows(bodies)})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// `primaries.get` — fetch the (id, timestamp, data) rows for a key.
pub fn primaries_get(duration: &str, key: &str) -> Result<Value, Error> {
    get_inner("v2/primaries.get", duration, key, /* telemetry = */ false)
}

/// `primaries.get.telemetry` — same shape but `value` instead of `data`.
pub fn primaries_get_telemetry(duration: &str, key: &str) -> Result<Value, Error> {
    get_inner("v2/primaries.get.telemetry", duration, key, /* telemetry = */ true)
}

fn get_inner(method: &'static str, duration: &str, key: &str, telemetry: bool) -> Result<Value, Error> {
    let merged = dispatch::read(
        method,
        json!({"session": "", "duration": duration, "key": key}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::primaries_get: get_db: {e}")))?;
            let arr: Vec<serde_json::Value> = if telemetry {
                db.primaries_get_telemetry(duration, key)
                    .map_err(|e| err_msg(format!("vm::api::primaries_get_telemetry: db: {e}")))?
                    .into_iter().map(|(id, ts, value)| json!({
                        "id": id.to_string(), "timestamp": ts, "value": value,
                    })).collect()
            } else {
                db.primaries_get(duration, key)
                    .map_err(|e| err_msg(format!("vm::api::primaries_get: db: {e}")))?
                    .into_iter().map(|(id, ts, data)| json!({
                        "id": id.to_string(), "timestamp": ts, "data": data,
                    })).collect()
            };
            Ok(json!({"results": arr}))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            let mut merged = merge::dedup_by_id(bodies, "results");
            merged.sort_by(|a, b| {
                let ta = a.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
                let tb = b.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
                tb.cmp(&ta)
            });
            json!({"results": merged})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Secondary IDs attached to a primary record.  Local-only — the
/// linkage lives on the same node as the primary.
pub fn secondaries(primary_id: Value) -> Result<Value, Error> {
    let pid_str = primary_id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::secondaries: id cast_string: {e}")))?;
    let uuid = Uuid::parse_str(&pid_str)
        .map_err(|e| err_msg(format!("vm::api::secondaries: parse id: {e}")))?;
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::secondaries: get_db: {e}")))?;
        let shard = find_shard_for_uuid(db, uuid)?;
        let ids: Vec<String> = shard.observability()
            .list_secondaries(uuid)
            .map_err(|e| err_msg(format!("vm::api::secondaries: list: {e}")))?
            .into_iter().map(|u| u.to_string()).collect();
        Ok(json!({"ids": ids}))
    })?;
    Ok(json_to_dynamic(result))
}

/// Fetch a primary record by id (with `secondaries_count` and
/// `duplications` injected, matching the v2/primary shape).  Local-only.
pub fn primary(id: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::primary: id cast_string: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::primary: parse id: {e}")))?;
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::primary: get_db: {e}")))?;
        let shard = find_shard_for_uuid(db, uuid)?;
        let obs = shard.observability();
        let mut doc = obs.get_by_id(uuid)
            .map_err(|e| err_msg(format!("vm::api::primary: get_by_id: {e}")))?
            .ok_or_else(|| err_msg(format!("primary {id_str} not found")))?;
        let secondaries_count = obs.list_secondaries(uuid).map(|v| v.len()).unwrap_or(0);
        let duplications: Vec<u64> = obs.get_duplicate_timestamps_by_id(uuid)
            .unwrap_or_default()
            .iter()
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
            .collect();
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("secondaries_count".into(), json!(secondaries_count));
            obj.insert("duplications".into(),      json!(duplications));
        }
        Ok(doc)
    })?;
    Ok(json_to_dynamic(result))
}

/// Fetch a secondary record by id (with `primary_id` and `duplications`
/// injected).  Local-only.
pub fn secondary(id: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::secondary: id cast_string: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::secondary: parse id: {e}")))?;
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::secondary: get_db: {e}")))?;
        let shard = find_shard_for_uuid(db, uuid)?;
        let obs = shard.observability();
        let mut doc = obs.get_by_id(uuid)
            .map_err(|e| err_msg(format!("vm::api::secondary: get_by_id: {e}")))?
            .ok_or_else(|| err_msg(format!("secondary {id_str} not found")))?;
        let primary_id = obs.primary_of(uuid)
            .map_err(|e| err_msg(format!("vm::api::secondary: primary_of: {e}")))?
            .ok_or_else(|| err_msg(format!("no primary for secondary {id_str}")))?;
        let duplications: Vec<u64> = obs.get_duplicate_timestamps_by_id(uuid)
            .unwrap_or_default()
            .iter()
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
            .collect();
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("primary_id".into(),   json!(primary_id.to_string()));
            obj.insert("duplications".into(), json!(duplications));
        }
        Ok(doc)
    })?;
    Ok(json_to_dynamic(result))
}

/// Locate the [`Shard`](crate::Shard) that contains `uuid`.  Mirrors
/// the bdsnode `params::find_shard_for_uuid` fast-path-then-scan logic.
fn find_shard_for_uuid(db: &crate::ShardsManager, uuid: Uuid) -> Result<crate::Shard, Error> {
    let cache = db.cache();
    let info  = cache.info();

    if let Some(ts) = crate::common::uuid::timestamp_from_v7(uuid) {
        if let Ok(infos) = info.shards_at(ts) {
            for si in infos {
                if let Ok(shard) = cache.shard(si.start_time) {
                    if shard.observability().get_by_id(uuid).ok().flatten().is_some() {
                        return Ok(shard);
                    }
                }
            }
        }
    }

    let all = info.list_all()
        .map_err(|e| err_msg(format!("vm::api: list shards: {e}")))?;
    for si in all {
        let shard = cache.shard(si.start_time)
            .map_err(|e| err_msg(format!("vm::api: shard: {e}")))?;
        if shard.observability().get_by_id(uuid).ok().flatten().is_some() {
            return Ok(shard);
        }
    }
    Err(err_msg(format!("record {uuid} not found in any shard")))
}
