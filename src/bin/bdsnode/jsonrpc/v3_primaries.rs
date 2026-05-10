//! Cluster-aware `v3/primaries*` family.
//!
//! - `v3/primaries`: sorted union of UUID strings.
//! - `v3/primaries.explore` + `v3/primaries.explore.telemetry`: per-key
//!   count merge (sum of counts) with `primary_id[]` UUID-set union.
//! - `v3/primaries.get` + `v3/primaries.get.telemetry`: UUID-deduped
//!   `(id, timestamp, data|value)` rows; first-seen wins.

use super::params::{rpc_err, v3_cluster_meta, TimeWindow, TimeWindowParams};
use super::v3_merge;
use bdslib::cluster::fanout;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet};

pub fn register(module: &mut RpcModule<()>) {
    register_primaries(module);
    register_explore(module, "v3/primaries.explore",           "v2/primaries.explore");
    register_explore(module, "v3/primaries.explore.telemetry", "v2/primaries.explore.telemetry");
    register_get(module,     "v3/primaries.get",               "v2/primaries.get",           "data");
    register_get(module,     "v3/primaries.get.telemetry",     "v2/primaries.get.telemetry", "value");
}

// ── v3/primaries ──────────────────────────────────────────────────────────────

fn register_primaries(module: &mut RpcModule<()>) {
    module.register_async_method("v3/primaries", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
        let p: TimeWindowParams = serde_json::from_value(raw.clone())
            .map_err(|e| rpc_err(-32602, format!("invalid window: {e}")))?;
        let window = p.resolve()?;

        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let cache = db.cache();
            let infos = match &window {
                TimeWindow::All           => cache.info().list_all(),
                TimeWindow::Range(s, e)   => cache.info().shards_in_range(*s, *e),
            }.map_err(|e| rpc_err(-32002, e))?;
            let mut ids: Vec<String> = Vec::new();
            for si in infos {
                let s = cache.shard(si.start_time).map_err(|e| rpc_err(-32003, e))?;
                let obs = s.observability();
                let uuids = match &window {
                    TimeWindow::All         => obs.list_primaries(),
                    TimeWindow::Range(a, b) => obs.list_primaries_in_range(*a, *b),
                }.map_err(|e| rpc_err(-32004, e))?;
                ids.extend(uuids.into_iter().map(|u| u.to_string()));
            }
            Ok(serde_json::json!({ "ids": ids }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/primaries", raw).await),
            None    => None,
        };

        // Sorted union of UUID strings.
        let mut bodies: Vec<&JsonValue> = vec![&local];
        if let Some(f) = &fan { bodies.extend(f.ok_results()); }
        let mut set: BTreeSet<String> = BTreeSet::new();
        for body in &bodies {
            if let Some(arr) = body.get("ids").and_then(|v| v.as_array()) {
                for v in arr {
                    if let Some(s) = v.as_str() { set.insert(s.to_owned()); }
                }
            }
        }
        let ids: Vec<String> = set.into_iter().collect();
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "ids": ids, "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}

// ── v3/primaries.explore (+ telemetry) ────────────────────────────────────────

fn register_explore(module: &mut RpcModule<()>, v3_method: &'static str, v2_method: &'static str) {
    module.register_async_method(v3_method, move |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
        let duration = raw.get("duration").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'duration'"))?
            .to_owned();

        let session = raw.get("session").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let v2_params = serde_json::json!({"session": session, "duration": duration.clone()});

        // Local call (matches the corresponding v2 ShardsManager method).
        let v2_method_local = v2_method;
        let dur_local = duration.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            humantime::parse_duration(&dur_local)
                .map_err(|e| rpc_err(-32600, format!("invalid duration {dur_local:?}: {e}")))?;
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let entries = if v2_method_local == "v2/primaries.explore" {
                db.primaries_explore(&dur_local).map_err(|e| rpc_err(-32004, e))?
            } else {
                db.primaries_explore_telemetry(&dur_local).map_err(|e| rpc_err(-32004, e))?
            };
            let items: Vec<JsonValue> = entries.into_iter().map(|(key, count, ids)| {
                serde_json::json!({
                    "key":        key,
                    "count":      count,
                    "primary_id": ids.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                })
            }).collect();
            Ok(JsonValue::Array(items))   // matches v2 shape (bare array)
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, v2_method, v2_params).await),
            None    => None,
        };

        // Merge per-key: sum counts, union primary_id sets.
        struct Acc { count: u64, ids: BTreeSet<String> }
        let mut by_key: BTreeMap<String, Acc> = BTreeMap::new();
        let mut bodies: Vec<&JsonValue> = vec![&local];
        if let Some(f) = &fan { bodies.extend(f.ok_results()); }
        for item in v3_merge::extract_arrays(bodies, "results") {
            let key = match item.get("key").and_then(|v| v.as_str()) {
                Some(s) => s.to_owned(), None => continue,
            };
            let entry = by_key.entry(key).or_insert_with(|| Acc { count: 0, ids: BTreeSet::new() });
            // Some peers may report different counts because secondaries
            // (post-dedup) can land on different sides of replication;
            // we sum them.  The UUID set tightens this on read.
            entry.count = entry.count.saturating_add(
                item.get("count").and_then(|v| v.as_u64()).unwrap_or(0));
            for u in item.get("primary_id").and_then(|v| v.as_array()).into_iter().flatten() {
                if let Some(s) = u.as_str() { entry.ids.insert(s.to_owned()); }
            }
        }

        let mut items: Vec<JsonValue> = by_key.into_iter().map(|(key, a)| {
            serde_json::json!({
                "key":        key,
                "count":      a.ids.len() as u64,  // dedup'd count via UUID set
                "raw_count":  a.count,             // sum of per-peer counts (replicated)
                "primary_id": a.ids.into_iter().collect::<Vec<_>>(),
            })
        }).collect();
        items.sort_by(|a, b| {
            let ca = a.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
            let cb = b.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
            cb.cmp(&ca)
        });

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "results":      items,
            "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}

// ── v3/primaries.get (+ telemetry) ────────────────────────────────────────────

fn register_get(module: &mut RpcModule<()>, v3_method: &'static str, v2_method: &'static str, _payload_field: &'static str) {
    module.register_async_method(v3_method, move |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
        let duration = raw.get("duration").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'duration'"))?.to_owned();
        let key = raw.get("key").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'key'"))?.to_owned();

        let session = raw.get("session").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let v2_params = serde_json::json!({"session": session, "duration": duration.clone(), "key": key.clone()});

        let v2_local = v2_method;
        let dur_local = duration.clone();
        let key_local = key.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            humantime::parse_duration(&dur_local)
                .map_err(|e| rpc_err(-32600, format!("invalid duration {dur_local:?}: {e}")))?;
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let items: Vec<JsonValue> = if v2_local == "v2/primaries.get" {
                db.primaries_get(&dur_local, &key_local).map_err(|e| rpc_err(-32004, e))?
                    .into_iter().map(|(id, ts, data)| serde_json::json!({
                        "id": id.to_string(), "timestamp": ts, "data": data,
                    })).collect()
            } else {
                db.primaries_get_telemetry(&dur_local, &key_local).map_err(|e| rpc_err(-32004, e))?
                    .into_iter().map(|(id, ts, value)| serde_json::json!({
                        "id": id.to_string(), "timestamp": ts, "value": value,
                    })).collect()
            };
            Ok(serde_json::json!({ "results": items }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, v2_method, v2_params).await),
            None    => None,
        };
        let mut bodies: Vec<&JsonValue> = vec![&local];
        if let Some(f) = &fan { bodies.extend(f.ok_results()); }
        let mut merged = v3_merge::dedup_by_id(bodies, "results");
        // Sort by timestamp descending for stable output.
        merged.sort_by(|a, b| {
            let ta = a.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
            let tb = b.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
            tb.cmp(&ta)
        });

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "results": merged, "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}
