//! `v3/tpl.*` — cluster-wide template-store reads.
//!
//! Templates live in per-shard `tplstorage` instances, and shards are
//! per-node, so different peers can have different template subsets.
//! Fan-out + UUID dedup gives the operator the full cluster-wide view.
//!
//! Methods:
//! - `v3/tpl.list`              — UUID dedup with first-seen wins.
//! - `v3/tpl.search`            — UUID dedup + score average.
//! - `v3/tpl.get`               — first non-null peer wins.
//! - `v3/tpl.template_by_id`    — first non-null peer wins.
//! - `v3/tpl.templates_recent`  — UUID dedup with first-seen wins.
//! - `v3/tpl.templates_by_timestamp` — same.

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::{fanout, merge};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

pub fn register(module: &mut RpcModule<()>) {
    register_list(module);
    register_search(module);
    register_get(module);
    register_template_by_id(module);
    register_templates_recent(module);
    register_templates_by_timestamp(module);
}

fn register_list(module: &mut RpcModule<()>) {
    module.register_async_method("v3/tpl.list", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
        let dur = raw.get("duration").and_then(|v| v.as_str()).unwrap_or("1h").to_owned();

        let dur_local = dur.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let all = db.tpl_list(&dur_local).map_err(|e| rpc_err(-32011, e))?;
            let templates: Vec<JsonValue> = all.into_iter().map(|(id, metadata)| {
                serde_json::json!({"id": id.to_string(), "metadata": metadata})
            }).collect();
            Ok(serde_json::json!({"templates": templates}))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/tpl.list", raw).await),
            None    => None,
        };
        let bodies = merge::bodies_from(&local, fan.as_ref());
        let templates = merge::dedup_by_id(bodies, "templates");
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "templates": templates, "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}

fn register_search(module: &mut RpcModule<()>) {
    module.register_async_method("v3/tpl.search", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
        let query = raw.get("query").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'query'"))?.to_owned();
        let dur   = raw.get("duration").and_then(|v| v.as_str()).unwrap_or("1h").to_owned();
        let limit = raw.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        let q_local = query.clone();
        let dur_local = dur.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let results = db.tpl_search_text(&dur_local, &q_local, limit)
                .map_err(|e| rpc_err(-32011, e))?;
            Ok(serde_json::json!({"results": results}))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/tpl.search", raw).await),
            None    => None,
        };
        let bodies = merge::bodies_from(&local, fan.as_ref());
        let mut results = merge::dedup_avg_score(bodies, "results");
        results.truncate(limit);
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "results": results, "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}

/// First-non-null-peer-wins fetch shared by `v3/tpl.get` and
/// `v3/tpl.template_by_id`.  `field` is the response field that holds
/// the (possibly-null) record.
async fn first_non_null_get(
    method: &'static str,
    v2_method: &'static str,
    raw: JsonValue,
    field: &str,
    fallback_local: impl FnOnce(JsonValue) -> tokio::task::JoinHandle<Result<JsonValue, ErrorObject<'static>>>,
) -> Result<JsonValue, ErrorObject<'static>> {
    let _ = method;
    let local = fallback_local(raw.clone()).await
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

    let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
    let fan = match &cluster {
        Some(c) => Some(fanout::fan_out_v2(c, v2_method, raw).await),
        None    => None,
    };

    // Local first; then any peer whose response has a non-null `field`.
    let local_has = local.get(field).map(|v| !v.is_null()).unwrap_or(false);
    let chosen: JsonValue = if local_has {
        local
    } else {
        let mut chosen = local.clone();
        if let Some(f) = &fan {
            for r in f.ok_results() {
                if r.get(field).map(|v| !v.is_null()).unwrap_or(false) {
                    chosen = r.clone();
                    break;
                }
            }
        }
        chosen
    };

    let mut out = chosen;
    if let Some(obj) = out.as_object_mut() {
        obj.insert("cluster_meta".into(), v3_cluster_meta(fan));
    }
    Ok(out)
}

fn register_get(module: &mut RpcModule<()>) {
    module.register_async_method("v3/tpl.get", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
        let id_str = raw.get("id").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'id'"))?.to_owned();
        let id = uuid::Uuid::parse_str(&id_str)
            .map_err(|e| rpc_err(-32600, format!("invalid id: {e}")))?;

        first_non_null_get("v3/tpl.get", "v2/tpl.get", raw, "metadata", move |_| {
            tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                match db.tpl_get_metadata(id).map_err(|e| rpc_err(-32011, e))? {
                    Some(meta) => {
                        let body = db.tpl_get_body(id).map_err(|e| rpc_err(-32011, e))?
                            .map(|b| String::from_utf8_lossy(&b).into_owned())
                            .unwrap_or_default();
                        Ok(serde_json::json!({"id": id.to_string(), "metadata": meta, "body": body}))
                    }
                    None => Ok(serde_json::json!({"id": id.to_string(), "metadata": null, "body": ""})),
                }
            })
        }).await
    }).unwrap();
}

fn register_template_by_id(module: &mut RpcModule<()>) {
    module.register_async_method("v3/tpl.template_by_id", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
        let id_str = raw.get("id").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing 'id'"))?.to_owned();

        first_non_null_get("v3/tpl.template_by_id", "v2/tpl.template_by_id", raw, "template", move |_| {
            let id_str = id_str.clone();
            tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let template = db.template_by_id(&id_str).map_err(|e| rpc_err(-32011, e))?;
                Ok(serde_json::json!({"template": template}))
            })
        }).await
    }).unwrap();
}

fn register_templates_recent(module: &mut RpcModule<()>) {
    module.register_async_method("v3/tpl.templates_recent", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
        let dur = raw.get("duration").and_then(|v| v.as_str()).unwrap_or("1h").to_owned();
        let dur_local = dur.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let templates = db.templates_recent(&dur_local).map_err(|e| rpc_err(-32011, e))?;
            Ok(serde_json::json!({"templates": templates}))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/tpl.templates_recent", raw).await),
            None    => None,
        };
        let bodies = merge::bodies_from(&local, fan.as_ref());
        let templates = merge::dedup_by_id(bodies, "templates");
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "templates": templates, "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}

fn register_templates_by_timestamp(module: &mut RpcModule<()>) {
    module.register_async_method("v3/tpl.templates_by_timestamp", |params, _ctx, _| async move {
        let raw: JsonValue = params.parse().unwrap_or(serde_json::json!({}));
        let start_ts = raw.get("start_ts").and_then(|v| v.as_u64())
            .ok_or_else(|| rpc_err(-32602, "missing 'start_ts'"))?;
        let end_ts = raw.get("end_ts").and_then(|v| v.as_u64())
            .ok_or_else(|| rpc_err(-32602, "missing 'end_ts'"))?;

        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let templates = db.templates_by_timestamp(start_ts, end_ts)
                .map_err(|e| rpc_err(-32011, e))?;
            Ok(serde_json::json!({"templates": templates}))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/tpl.templates_by_timestamp", raw).await),
            None    => None,
        };
        let bodies = merge::bodies_from(&local, fan.as_ref());
        let templates = merge::dedup_by_id(bodies, "templates");
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "templates": templates, "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}
