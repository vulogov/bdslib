//! `v3/keys`, `v3/keys.all`, `v3/keys.get` — cluster-wide key enumeration.
//!
//! - `v3/keys` and `v3/keys.all` return a deduplicated, sorted union of
//!   string arrays from every peer's `keys[]` response.
//! - `v3/keys.get` returns deduplicated `{primary_id, timestamp,
//!   secondary_ids}` rows; per-key UUID dedup, secondaries unioned per
//!   primary_id.

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::{fanout, merge};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use std::collections::BTreeSet;

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
        let bodies = merge::bodies_from(&local, fan.as_ref());
        let keys = merge::union_strings(bodies, "keys");
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
        let bodies = merge::bodies_from(&local, fan.as_ref());
        let keys = merge::union_strings(bodies, "keys");
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

        let bodies = merge::bodies_from(&local, fan.as_ref());
        let results = merge::merge_keys_get_rows(bodies);
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
