//! Telemetry-key enumeration: `keys`, `keys_all`, `keys_get`.
//!
//! All cluster-aware reads.  Merge strategies match the bdsnode
//! `v3/keys*` handlers:
//!
//! | Helper      | Merge                                                |
//! |-------------|------------------------------------------------------|
//! | `keys`      | sorted union of strings (`keys[]`)                   |
//! | `keys_all`  | sorted union of strings (`keys[]`)                   |
//! | `keys_get`  | per-`primary_id`, union `secondary_ids`, ts-DESC     |

use crate::cluster::merge;
use crate::vm::api::dispatch;
use crate::vm::helpers::eval::json_to_dynamic;
use easy_error::{err_msg, Error};
use rust_dynamic::value::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

/// Distinct primary keys observed in the trailing `duration` window.
pub fn keys(duration: &str) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/keys",
        json!({"session": "", "duration": duration}),
        move || {
            let secs = humantime::parse_duration(duration)
                .map_err(|e| err_msg(format!("vm::api::keys: parse duration {duration:?}: {e}")))?
                .as_secs();
            let end   = SystemTime::now();
            let start = end - Duration::from_secs(secs);

            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::keys: get_db: {e}")))?;
            let cache = db.cache();
            let infos = cache.info().shards_in_range(start, end)
                .map_err(|e| err_msg(format!("vm::api::keys: shards: {e}")))?;
            let mut keys: BTreeSet<String> = BTreeSet::new();
            for si in infos {
                let s = cache.shard(si.start_time)
                    .map_err(|e| err_msg(format!("vm::api::keys: shard: {e}")))?;
                keys.extend(s.observability().list_primary_keys_in_range(start, end)
                    .map_err(|e| err_msg(format!("vm::api::keys: list keys: {e}")))?);
            }
            Ok(json!({"keys": keys.into_iter().collect::<Vec<_>>()}))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            json!({"keys": merge::union_strings(bodies, "keys")})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Pattern-filtered key enumeration via `db.keys_all(duration, pattern)`.
/// `pattern` defaults to `"*"` if you pass an empty string.
pub fn keys_all(duration: &str, pattern: &str) -> Result<Value, Error> {
    let pat = if pattern.is_empty() { "*".to_owned() } else { pattern.to_owned() };
    let merged = dispatch::read(
        "v2/keys.all",
        json!({"session": "", "duration": duration, "key": pat.clone()}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::keys_all: get_db: {e}")))?;
            let keys = db.keys_all(duration, &pat)
                .map_err(|e| err_msg(format!("vm::api::keys_all: db: {e}")))?;
            Ok(json!({"keys": keys}))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            json!({"keys": merge::union_strings(bodies, "keys")})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Per-key UUID enumeration: `{primary_id, timestamp, secondary_ids}`
/// rows.  Cluster-merged via `merge::merge_keys_get_rows` (per
/// primary_id union of secondary_ids, sort by timestamp DESC).
pub fn keys_get(duration: &str, key: &str) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/keys.get",
        json!({"session": "", "duration": duration, "key": key}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::keys_get: get_db: {e}")))?;
            let entries = db.keys_by_pattern(duration, key)
                .map_err(|e| err_msg(format!("vm::api::keys_get: db: {e}")))?;
            let arr: Vec<serde_json::Value> = entries.into_iter().map(|(pid, ts, sids)| {
                json!({
                    "primary_id":    pid.to_string(),
                    "timestamp":     ts,
                    "secondary_ids": sids.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                })
            }).collect();
            Ok(json!({"results": arr}))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            json!({"results": merge::merge_keys_get_rows(bodies)})
        },
    )?;
    Ok(json_to_dynamic(merged))
}
