//! Add / update / delete / count / duplicates / fingerprints — the
//! observability ingest + counting surface.
//!
//! Cluster behaviour:
//!
//! | Helper                | Cluster strategy                                |
//! |-----------------------|-------------------------------------------------|
//! | `add`                 | local + sharded fan-out (`replication_factor`)  |
//! | `add_batch`           | local + sharded fan-out via `v2/add.batch`      |
//! | `update`              | local-only (no v3 fan-out exists)               |
//! | `delete_by_id`        | local-only                                      |
//! | `count`               | read: sum per-peer counts                       |
//! | `duplicates`          | local-only                                      |
//! | `fingerprints_recent` | read: UUID-dedup union                          |

use crate::cluster::merge;
use crate::vm::api::dispatch;
use crate::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use easy_error::{bail, err_msg, Error};
use rust_dynamic::value::Value;
use serde_json::json;
use std::time::UNIX_EPOCH;
use uuid::Uuid;

/// Add a single observability record.  Returns the new record's UUID
/// as a `Value::String`.  In cluster mode the record also lands on
/// `replication_factor - 1` random Alive peers; failures hint.
pub fn add(doc: Value) -> Result<Value, Error> {
    let doc_json = dynamic_to_json(doc);
    let id_str = dispatch::write_sharded(
        "v2/add",
        json!({"doc": doc_json}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::add: get_db: {e}")))?;
            let payload = match crate::vm::api::add::take_doc(&json!({"doc": &doc_json})) {
                Some(d) => d,
                None    => doc_json.clone(),
            };
            let id = db.add(payload)
                .map_err(|e| err_msg(format!("vm::api::add: db.add: {e}")))?;
            Ok(id.to_string())
        },
        |_id, _params| {
            // The receiving v2/add doesn't accept an injected id — it
            // mints its own UUIDv7.  This means each replica gets a
            // distinct id.  Matches the existing v3/add behaviour:
            // sharded *records*, not sharded ids.
        },
    )?;
    Ok(Value::from_string(id_str))
}

/// Add a batch of observability records.  Returns a `Value::List` of
/// UUID strings (one per input record).
pub fn add_batch(docs: Value) -> Result<Value, Error> {
    let docs_json = dynamic_to_json(docs);
    let arr = match docs_json.as_array() {
        Some(a) => a.clone(),
        None    => bail!("vm::api::add_batch: docs must be a list"),
    };
    let db = crate::globals::get_db()
        .map_err(|e| err_msg(format!("vm::api::add_batch: get_db: {e}")))?;
    // Local commit first.
    let ids = db.add_batch(arr.clone())
        .map_err(|e| err_msg(format!("vm::api::add_batch: db.add_batch: {e}")))?;
    let id_strings: Vec<String> = ids.iter().map(|u| u.to_string()).collect();

    // Cluster fan-out.  Mirror the local payload to peers via v2/add.batch.
    if let Some(cluster) = db.cluster() {
        let cluster = cluster.clone();
        let params = json!({"docs": arr});
        let outcome = crate::vm::api::runtime::block_on(
            crate::cluster::replication::replicate_to_all(cluster, "v2/add.batch", params),
        );
        crate::vm::api::meta::set(json!({
            "enabled":     true,
            "replication": outcome.to_json(),
        }));
    } else {
        crate::vm::api::meta::clear();
    }

    Ok(Value::from_list(
        id_strings.into_iter().map(Value::from_string).collect(),
    ))
}

/// Update the JSON document of an existing record.  No cluster fan-out
/// exists for updates; the call goes to the local DB only.
pub fn update(id: Value, doc: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::update: id cast_string: {e}")))?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::update: parse id: {e}")))?;
    let doc_json = dynamic_to_json(doc);
    let new_id = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::update: get_db: {e}")))?;
        db.update(id, doc_json)
            .map_err(|e| err_msg(format!("vm::api::update: db.update: {e}")))
    })?;
    Ok(Value::from_string(new_id.to_string()))
}

/// Delete a record by id.  Local-only; cluster replicas are not
/// notified (no v3/delete in the JSON-RPC surface either).  Returns
/// `Value::nodata` on success.
pub fn delete_by_id(id: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::delete: id cast_string: {e}")))?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::delete: parse id: {e}")))?;
    dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::delete: get_db: {e}")))?;
        db.delete_by_id(id)
            .map_err(|e| err_msg(format!("vm::api::delete: db.delete_by_id: {e}")))
    })?;
    Ok(Value::nodata())
}

/// Total record count.  In cluster mode this returns the **sum** of
/// per-peer counts (overcounts replicated records by ≈
/// `replication_factor`).  For a true distinct count, callers should
/// use the JSON-RPC `v3/count?distinct=true` from outside Bund.
///
/// `opts` may be `Value::nodata` (count everything) or a Map with
/// `duration: "1h"` or `start_ts` + `end_ts` Unix-second bounds.
pub fn count(opts: Value) -> Result<Value, Error> {
    let opts_json = dynamic_to_json(opts);
    let merged = dispatch::read(
        "v2/count",
        opts_json.clone(),
        || local_count(&opts_json).map(|n| json!({"count": n})),
        |local, fan| {
            let local_n = local.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
            let total = merge::sum_field(local_n, fan, "count");
            json!({ "count": total, "local_count": local_n })
        },
    )?;
    Ok(json_to_dynamic(merged))
}

fn local_count(opts: &serde_json::Value) -> Result<u64, Error> {
    use crate::vm::api::time_window::resolve_window;
    let window = resolve_window(opts)?;
    let db = crate::globals::get_db()
        .map_err(|e| err_msg(format!("vm::api::count: get_db: {e}")))?;
    let cache = db.cache();
    let infos = window.list_shards(cache.info())
        .map_err(|e| err_msg(format!("vm::api::count: list shards: {e}")))?;
    let mut total = 0u64;
    for si in infos {
        let s = cache.shard(si.start_time)
            .map_err(|e| err_msg(format!("vm::api::count: shard: {e}")))?;
        let n = match window {
            crate::vm::api::time_window::Window::All => s.observability().count_all(),
            crate::vm::api::time_window::Window::Range(start, end) => {
                s.observability().count_in_range(start, end)
            }
        }.map_err(|e| err_msg(format!("vm::api::count: count: {e}")))?;
        total += n;
    }
    Ok(total)
}

/// Duplication observations (exact-match within shards).  Local-only.
/// Returns a `Value::Map` of `id_string → list_of_unix_seconds`.
pub fn duplicates(opts: Value) -> Result<Value, Error> {
    use crate::vm::api::time_window::resolve_window;
    let opts_json = dynamic_to_json(opts);
    let window = resolve_window(&opts_json)?;
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::duplicates: get_db: {e}")))?;
        let cache = db.cache();
        let infos = window.list_shards(cache.info())
            .map_err(|e| err_msg(format!("vm::api::duplicates: list shards: {e}")))?;
        let mut out = serde_json::Map::new();
        for si in infos {
            let shard = cache.shard(si.start_time)
                .map_err(|e| err_msg(format!("vm::api::duplicates: shard: {e}")))?;
            let entries = match window {
                crate::vm::api::time_window::Window::All => shard.observability().list_all_dedup_entries(),
                crate::vm::api::time_window::Window::Range(s, e) => {
                    shard.observability().list_dedup_entries_in_range(s, e)
                }
            }.map_err(|e| err_msg(format!("vm::api::duplicates: entries: {e}")))?;
            for (id, _key, times) in entries {
                if times.is_empty() { continue; }
                let ts: Vec<u64> = times.into_iter()
                    .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
                    .collect();
                out.entry(id.to_string()).or_insert_with(|| json!(ts));
            }
        }
        Ok(serde_json::Value::Object(out))
    })?;
    Ok(json_to_dynamic(result))
}

/// Recent fingerprints with their record ids.  Cluster-aware: the
/// returned list is the UUID-deduped union of every Alive peer's
/// `fingerprints_with_ids_in_recent` plus the local set.  Each output
/// item is a Map `{id: "uuid", fingerprint: "…"}`.
pub fn fingerprints_recent(duration: &str) -> Result<Value, Error> {
    let dur = humantime::parse_duration(duration)
        .map_err(|e| err_msg(format!("vm::api::fingerprints_recent: parse duration {duration:?}: {e}")))?;
    let merged = dispatch::read(
        "v2/fingerprints.recent",
        json!({"duration": duration}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::fingerprints_recent: get_db: {e}")))?;
            let pairs = db.fingerprints_with_ids_in_recent(dur)
                .map_err(|e| err_msg(format!("vm::api::fingerprints_recent: db: {e}")))?;
            let arr: Vec<serde_json::Value> = pairs.into_iter().map(|(id, fp)| {
                json!({"id": id.to_string(), "fingerprint": fp})
            }).collect();
            Ok(json!({"fingerprints": arr}))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            let fps = merge::dedup_by_id(bodies, "fingerprints");
            json!({"fingerprints": fps})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Internal: extract the `doc` field from the v2/add params shape if
/// present.  Used to share params construction between local and
/// fan-out paths in [`add`].  Returns `None` when the params don't
/// follow the conventional shape, in which case the caller falls back
/// to its source value.
#[doc(hidden)]
pub(crate) fn take_doc(params: &serde_json::Value) -> Option<serde_json::Value> {
    params.get("doc").cloned()
}
