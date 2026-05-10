//! `v3/keys`, `v3/keys.all`, `v3/keys.get` — cluster-wide key enumeration.
//!
//! - `v3/keys` and `v3/keys.all` return a deduplicated, sorted union of
//!   string arrays from every peer's `keys[]` response.
//! - `v3/keys.get` returns deduplicated `{primary_id, timestamp,
//!   secondary_ids}` rows; per-key UUID dedup, secondaries unioned per
//!   primary_id.

use super::params::{rpc_err, v3_cluster_meta};
use super::v3_merge;
use bdslib::cluster::fanout;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use std::collections::BTreeSet;
use std::collections::HashMap;

pub fn register(module: &mut RpcModule<()>) {
    register_keys(module);
    register_keys_all(module);
    register_keys_get(module);
}

#[derive(serde::Deserialize, Clone)]
struct KeysParams {
    #[serde(default)] session:  String,
    duration: String,
}

fn register_keys(module: &mut RpcModule<()>) {
    module.register_async_method("v3/keys", |params, _ctx, _| async move {
        let p: KeysParams = params.parse()?;
        let v2_params = serde_json::json!({"session": p.session, "duration": p.duration.clone()});

        let local = local_keys(&p.duration).await?;
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/keys", v2_params).await),
            None    => None,
        };
        let mut bodies: Vec<&JsonValue> = vec![&local];
        if let Some(f) = &fan { bodies.extend(f.ok_results()); }
        let keys = v3_merge::union_strings(bodies, "keys");
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "keys": keys, "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}

#[derive(serde::Deserialize, Clone)]
struct KeysAllParams {
    #[serde(default)] session: String,
    duration: String,
    #[serde(default = "default_pattern")]
    key: String,
}
fn default_pattern() -> String { "*".to_owned() }

fn register_keys_all(module: &mut RpcModule<()>) {
    module.register_async_method("v3/keys.all", |params, _ctx, _| async move {
        let p: KeysAllParams = params.parse()?;
        let v2_params = serde_json::json!({"session": p.session, "duration": p.duration.clone(), "key": p.key.clone()});
        let p_local = p.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let keys = db.keys_all(&p_local.duration, &p_local.key)
                .map_err(|e| rpc_err(-32004, e))?;
            Ok(serde_json::json!({ "keys": keys }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/keys.all", v2_params).await),
            None    => None,
        };
        let mut bodies: Vec<&JsonValue> = vec![&local];
        if let Some(f) = &fan { bodies.extend(f.ok_results()); }
        let keys = v3_merge::union_strings(bodies, "keys");
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "keys": keys, "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}

#[derive(serde::Deserialize, Clone)]
struct KeysGetParams {
    #[serde(default)] session: String,
    duration: String,
    key: String,
}

fn register_keys_get(module: &mut RpcModule<()>) {
    module.register_async_method("v3/keys.get", |params, _ctx, _| async move {
        let p: KeysGetParams = params.parse()?;
        let v2_params = serde_json::json!({"session": p.session, "duration": p.duration.clone(), "key": p.key.clone()});
        let p_local = p.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let entries = db.keys_by_pattern(&p_local.duration, &p_local.key)
                .map_err(|e| rpc_err(-32004, e))?;
            let results: Vec<JsonValue> = entries.into_iter().map(|(pid, ts, sids)| {
                serde_json::json!({
                    "primary_id":     pid.to_string(),
                    "timestamp":      ts,
                    "secondary_ids":  sids.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                })
            }).collect();
            Ok(serde_json::json!({ "results": results }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/keys.get", v2_params).await),
            None    => None,
        };

        // Merge: per primary_id, union secondary_ids; first-seen wins for ts.
        let mut bodies: Vec<&JsonValue> = vec![&local];
        if let Some(f) = &fan { bodies.extend(f.ok_results()); }
        let mut by_primary: HashMap<String, (i64, BTreeSet<String>)> = HashMap::new();
        for item in v3_merge::extract_arrays(bodies, "results") {
            let pid = match item.get("primary_id").and_then(|v| v.as_str()) {
                Some(s) => s.to_owned(), None => continue,
            };
            let ts  = item.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
            let entry = by_primary.entry(pid).or_insert_with(|| (ts, BTreeSet::new()));
            for s in item.get("secondary_ids").and_then(|v| v.as_array()).into_iter().flatten() {
                if let Some(s) = s.as_str() { entry.1.insert(s.to_owned()); }
            }
        }
        let mut results: Vec<JsonValue> = by_primary.into_iter().map(|(pid, (ts, sids))| {
            serde_json::json!({
                "primary_id":    pid,
                "timestamp":     ts,
                "secondary_ids": sids.into_iter().collect::<Vec<_>>(),
            })
        }).collect();
        // Sort by timestamp descending for stable output.
        results.sort_by(|a, b| {
            let ta = a.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
            let tb = b.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
            tb.cmp(&ta)
        });

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "results": results, "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}

async fn local_keys(duration: &str) -> Result<JsonValue, ErrorObject<'static>> {
    let dur = duration.to_owned();
    tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
        let secs = humantime::parse_duration(&dur)
            .map_err(|e| rpc_err(-32600, format!("invalid duration {dur:?}: {e}")))?
            .as_secs();
        use std::time::{Duration, SystemTime};
        let end = SystemTime::now();
        let start = end - Duration::from_secs(secs);

        let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
        let cache = db.cache();
        let infos = cache.info().shards_in_range(start, end)
            .map_err(|e| rpc_err(-32002, e))?;
        let mut keys: BTreeSet<String> = BTreeSet::new();
        for si in infos {
            let s = cache.shard(si.start_time).map_err(|e| rpc_err(-32003, e))?;
            keys.extend(s.observability().list_primary_keys_in_range(start, end)
                .map_err(|e| rpc_err(-32004, e))?);
        }
        Ok(serde_json::json!({ "keys": keys.into_iter().collect::<Vec<_>>() }))
    }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?
}
