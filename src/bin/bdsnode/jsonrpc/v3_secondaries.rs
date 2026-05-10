//! `v3/secondaries`, `v3/secondary` — cluster-wide.
//!
//! - `v3/secondaries`: per-primary, union the secondary-UUID sets that
//!   each peer reports.  Different peers can classify the same record as
//!   primary vs secondary differently (similarity_threshold is corpus-
//!   relative), so the unioned view is the most complete.
//! - `v3/secondary`: first-non-error-peer-wins fetch by UUID, mirroring
//!   the v3/tpl.{get,template_by_id} pattern.

use super::params::{rpc_err, v3_cluster_meta, find_shard_for_uuid, duplication_timestamps};
use bdslib::cluster::fanout;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use std::collections::BTreeSet;
use uuid::Uuid;

pub fn register(module: &mut RpcModule<()>) {
    register_secondaries(module);
    register_secondary(module);
}

fn register_secondaries(module: &mut RpcModule<()>) {
    module.register_async_method("v3/secondaries", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse()?;
        let primary_id = raw.get("primary_id").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'primary_id'"))?.to_owned();
        let _ = Uuid::parse_str(&primary_id)
            .map_err(|e| rpc_err(-32600, format!("invalid UUID {primary_id:?}: {e}")))?;

        // Local lookup — silently empty when this node doesn't host the primary.
        let pid_local = primary_id.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let uuid = Uuid::parse_str(&pid_local).unwrap();
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            // find_shard_for_uuid returns -32404 when the primary isn't on
            // this node.  Treat that as "no secondaries here" rather than
            // a hard error so fan-out can still produce the cluster view.
            match find_shard_for_uuid(uuid, db) {
                Ok(shard) => {
                    let ids: Vec<String> = shard.observability()
                        .list_secondaries(uuid)
                        .map_err(|e| rpc_err(-32004, e))?
                        .into_iter()
                        .map(|u| u.to_string())
                        .collect();
                    Ok(serde_json::json!({"ids": ids}))
                }
                Err(_) => Ok(serde_json::json!({"ids": []})),
            }
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/secondaries", raw).await),
            None    => None,
        };

        // Sorted union of secondary UUID strings.
        let mut set: BTreeSet<String> = BTreeSet::new();
        let mut bodies: Vec<&JsonValue> = vec![&local];
        if let Some(f) = &fan { bodies.extend(f.ok_results()); }
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

fn register_secondary(module: &mut RpcModule<()>) {
    module.register_async_method("v3/secondary", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse()?;
        let secondary_id = raw.get("secondary_id").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'secondary_id'"))?.to_owned();
        let uuid = Uuid::parse_str(&secondary_id)
            .map_err(|e| rpc_err(-32600, format!("invalid UUID {secondary_id:?}: {e}")))?;

        // Local lookup.
        let local: Option<JsonValue> = tokio::task::spawn_blocking(move || -> Option<JsonValue> {
            let db = match bdslib::get_db() { Ok(d) => d, Err(_) => return None };
            let shard = match find_shard_for_uuid(uuid, db) { Ok(s) => s, Err(_) => return None };
            let obs = shard.observability();
            let mut doc = match obs.get_by_id(uuid).ok().flatten() { Some(d) => d, None => return None };
            let primary_id = obs.primary_of(uuid).ok().flatten()?;
            let duplications = duplication_timestamps(obs, uuid);
            if let Some(obj) = doc.as_object_mut() {
                obj.insert("primary_id".to_string(), serde_json::json!(primary_id.to_string()));
                obj.insert("duplications".to_string(), serde_json::json!(duplications));
            }
            Some(doc)
        }).await.unwrap_or(None);

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/secondary", raw).await),
            None    => None,
        };

        // First non-empty wins: local first, then peers in fan-out completion order.
        let chosen: Option<JsonValue> = local.or_else(|| {
            fan.as_ref().and_then(|f| f.ok_results().next().cloned())
        });

        match chosen {
            Some(mut doc) => {
                if let Some(obj) = doc.as_object_mut() {
                    obj.insert("cluster_meta".into(), v3_cluster_meta(fan));
                }
                Ok::<JsonValue, ErrorObject>(doc)
            }
            None => Err(rpc_err(-32404, format!("secondary {secondary_id} not found in cluster"))),
        }
    }).unwrap();
}
