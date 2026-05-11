//! Per-shard template store: drain3 templates plus arbitrary blobs.
//!
//! Writes (`tpl_add` / `tpl_update_*` / `tpl_delete` / `tpl_reindex`)
//! are local-only — templates live inside the per-shard `tplstorage`
//! and aren't part of any fully-replicated store.  Reads are
//! cluster-aware via the `v3/tpl.*` fan-out methods.

use crate::cluster::merge;
use crate::vm::api::dispatch;
use crate::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use easy_error::{err_msg, Error};
use rust_dynamic::value::Value;
use serde_json::json;
use uuid::Uuid;

// ── writes (local-only) ──────────────────────────────────────────────────────

/// Add a template (`metadata` Map + `body` bytes).  Returns the new
/// template UUID as a `Value::String`.  Local-only.
pub fn tpl_add(metadata: Value, body: Vec<u8>) -> Result<Value, Error> {
    let meta_json = dynamic_to_json(metadata);
    let id = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::tpl_add: get_db: {e}")))?;
        db.tpl_add(meta_json, &body)
            .map_err(|e| err_msg(format!("vm::api::tpl_add: db: {e}")))
    })?;
    Ok(Value::from_string(id.to_string()))
}

/// Replace a template's metadata.  Local-only.
pub fn tpl_update_metadata(id: Value, metadata: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::tpl_update_metadata: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::tpl_update_metadata: parse id: {e}")))?;
    let meta_json = dynamic_to_json(metadata);
    dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::tpl_update_metadata: get_db: {e}")))?;
        db.tpl_update_metadata(uuid, meta_json)
            .map_err(|e| err_msg(format!("vm::api::tpl_update_metadata: db: {e}")))
    })?;
    Ok(Value::nodata())
}

/// Replace a template's body bytes.  Local-only.
pub fn tpl_update_body(id: Value, body: Vec<u8>) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::tpl_update_body: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::tpl_update_body: parse id: {e}")))?;
    dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::tpl_update_body: get_db: {e}")))?;
        db.tpl_update_body(uuid, &body)
            .map_err(|e| err_msg(format!("vm::api::tpl_update_body: db: {e}")))
    })?;
    Ok(Value::nodata())
}

/// Delete a template.  Local-only.
pub fn tpl_delete(id: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::tpl_delete: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::tpl_delete: parse id: {e}")))?;
    dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::tpl_delete: get_db: {e}")))?;
        db.tpl_delete(uuid)
            .map_err(|e| err_msg(format!("vm::api::tpl_delete: db: {e}")))
    })?;
    Ok(Value::nodata())
}

/// Re-embed every template across the (optional) duration window.
/// Returns the count re-embedded as a `Value::Int`.
pub fn tpl_reindex(duration: &str) -> Result<Value, Error> {
    let n = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::tpl_reindex: get_db: {e}")))?;
        db.tpl_reindex(duration)
            .map_err(|e| err_msg(format!("vm::api::tpl_reindex: db: {e}")))
    })?;
    Ok(Value::from_int(n as i64))
}

// ── reads (cluster-aware) ────────────────────────────────────────────────────

/// Fetch a template's `{metadata, body}` by UUID.  Cluster strategy:
/// first non-null peer wins (templates may live on a single node).
pub fn tpl_get(id: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::tpl_get: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::tpl_get: parse id: {e}")))?;
    let id_for_local = id_str.clone();
    let merged = dispatch::read(
        "v2/tpl.get",
        json!({"id": id_str}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::tpl_get: get_db: {e}")))?;
            match db.tpl_get_metadata(uuid)
                .map_err(|e| err_msg(format!("vm::api::tpl_get: db meta: {e}")))? {
                Some(meta) => {
                    let body = db.tpl_get_body(uuid)
                        .map_err(|e| err_msg(format!("vm::api::tpl_get: db body: {e}")))?
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default();
                    Ok(json!({"id": id_for_local, "metadata": meta, "body": body}))
                }
                None => Ok(json!({"id": id_for_local, "metadata": null, "body": ""})),
            }
        },
        |local, fan| first_non_null(&local, fan, "metadata"),
    )?;
    Ok(json_to_dynamic(merged))
}

/// All templates within `duration`.  Cluster merge: UUID dedup
/// (first-seen wins).
pub fn tpl_list(duration: &str) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/tpl.list",
        json!({"duration": duration}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::tpl_list: get_db: {e}")))?;
            let all = db.tpl_list(duration)
                .map_err(|e| err_msg(format!("vm::api::tpl_list: db: {e}")))?;
            let templates: Vec<serde_json::Value> = all.into_iter().map(|(id, metadata)| {
                json!({"id": id.to_string(), "metadata": metadata})
            }).collect();
            Ok(json!({"templates": templates}))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            json!({"templates": merge::dedup_by_id(bodies, "templates")})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Semantic search over template bodies.  Cluster merge: UUID dedup +
/// score average + truncate(limit).
pub fn tpl_search(duration: &str, query: &str, limit: usize) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/tpl.search",
        json!({"duration": duration, "query": query, "limit": limit}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::tpl_search: get_db: {e}")))?;
            let results = db.tpl_search_text(duration, query, limit)
                .map_err(|e| err_msg(format!("vm::api::tpl_search: db: {e}")))?;
            Ok(json!({"results": results}))
        },
        move |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            let mut results = merge::dedup_avg_score(bodies, "results");
            results.truncate(limit);
            json!({"results": results})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Fetch a single template by its template-id string (drain3 internal
/// id).  Cluster strategy: first non-null peer wins.
pub fn tpl_template_by_id(id: &str) -> Result<Value, Error> {
    let id_for_local = id.to_owned();
    let merged = dispatch::read(
        "v2/tpl.template_by_id",
        json!({"id": id}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::tpl_template_by_id: get_db: {e}")))?;
            let template = db.template_by_id(&id_for_local)
                .map_err(|e| err_msg(format!("vm::api::tpl_template_by_id: db: {e}")))?;
            Ok(json!({"template": template}))
        },
        |local, fan| first_non_null(&local, fan, "template"),
    )?;
    Ok(json_to_dynamic(merged))
}

/// Templates observed in the trailing `duration` window.  Cluster
/// merge: UUID dedup (first-seen wins).
pub fn tpl_templates_recent(duration: &str) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/tpl.templates_recent",
        json!({"duration": duration}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::tpl_templates_recent: get_db: {e}")))?;
            let templates = db.templates_recent(duration)
                .map_err(|e| err_msg(format!("vm::api::tpl_templates_recent: db: {e}")))?;
            Ok(json!({"templates": templates}))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            json!({"templates": merge::dedup_by_id(bodies, "templates")})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Templates observed inside an explicit `[start_ts, end_ts]` Unix-
/// second range.  Cluster merge: UUID dedup (first-seen wins).
pub fn tpl_templates_by_timestamp(start_ts: u64, end_ts: u64) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/tpl.templates_by_timestamp",
        json!({"start_ts": start_ts, "end_ts": end_ts}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::tpl_templates_by_timestamp: get_db: {e}")))?;
            let templates = db.templates_by_timestamp(start_ts, end_ts)
                .map_err(|e| err_msg(format!("vm::api::tpl_templates_by_timestamp: db: {e}")))?;
            Ok(json!({"templates": templates}))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            json!({"templates": merge::dedup_by_id(bodies, "templates")})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Pick the local body if it has a non-null `field`; otherwise the
/// first peer whose response does.  Used by tpl_get / tpl_template_by_id
/// — templates may live on only one node.
fn first_non_null(
    local: &serde_json::Value,
    fan:   Option<&crate::cluster::fanout::FanOutResults>,
    field: &str,
) -> serde_json::Value {
    if local.get(field).map(|v| !v.is_null()).unwrap_or(false) {
        return local.clone();
    }
    if let Some(f) = fan {
        for r in f.ok_results() {
            if r.get(field).map(|v| !v.is_null()).unwrap_or(false) {
                return r.clone();
            }
        }
    }
    local.clone()
}
