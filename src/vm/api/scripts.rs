//! Fully-replicated scripts store: add / update / delete / get / list.
//!
//! Writes replicate to every Alive peer; reads are local-only (anti-
//! entropy keeps replicas converged).

use crate::vm::api::dispatch;
use crate::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use easy_error::{err_msg, Error};
use rust_dynamic::value::Value;
use serde_json::json;
use uuid::Uuid;

/// Add a script.  `metadata` Map should include the conventional fields
/// (`name`, `language`, `updated_at`, …).  Returns the new script UUID.
pub fn script_add(metadata: Value, script: &str) -> Result<Value, Error> {
    let meta_json = dynamic_to_json(metadata);
    let script_owned = script.to_owned();
    let id_str = dispatch::write_replicated(
        "v2/script_add",
        json!({"metadata": meta_json.clone(), "script": script_owned.clone()}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::script_add: get_db: {e}")))?;
            let id = db.script_add(meta_json.clone(), &script_owned)
                .map_err(|e| err_msg(format!("vm::api::script_add: db: {e}")))?;
            Ok(id.to_string())
        },
        |id, params| {
            if let Some(obj) = params.as_object_mut() {
                obj.insert("id".into(), json!(id));
            }
        },
    )?;
    Ok(Value::from_string(id_str))
}

/// Update both metadata and body of an existing script.
pub fn script_update(id: Value, metadata: Value, script: &str) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::script_update: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::script_update: parse id: {e}")))?;
    let meta_json = dynamic_to_json(metadata);
    let script_owned = script.to_owned();
    let _ = dispatch::write_replicated(
        "v2/script_update",
        json!({"id": id_str, "metadata": meta_json.clone(), "script": script_owned.clone()}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::script_update: get_db: {e}")))?;
            db.update_script(uuid, meta_json.clone(), &script_owned)
                .map_err(|e| err_msg(format!("vm::api::script_update: db: {e}")))?;
            Ok(id_str.clone())
        },
        |_, _| {},
    )?;
    Ok(Value::nodata())
}

/// Delete a script.
pub fn script_delete(id: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::script_delete: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::script_delete: parse id: {e}")))?;
    let _ = dispatch::write_replicated(
        "v2/script_delete",
        json!({"id": id_str}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::script_delete: get_db: {e}")))?;
            db.script_delete(uuid)
                .map_err(|e| err_msg(format!("vm::api::script_delete: db: {e}")))?;
            Ok(id_str.clone())
        },
        |_, _| {},
    )?;
    Ok(Value::nodata())
}

/// Fetch a script's `{id, script, metadata}` Map by UUID.  Local-only.
/// Returns `Value::nodata()` when the script doesn't exist.
pub fn script_get(id: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::script_get: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::script_get: parse id: {e}")))?;
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::script_get: get_db: {e}")))?;
        let body = db.script(uuid)
            .map_err(|e| err_msg(format!("vm::api::script_get: db body: {e}")))?;
        let meta = db.script_metadata(uuid)
            .map_err(|e| err_msg(format!("vm::api::script_get: db meta: {e}")))?;
        match body {
            Some(b) => Ok(json!({
                "id": id_str,
                "script": b,
                "metadata": meta.unwrap_or(json!({})),
            })),
            None => Ok(serde_json::Value::Null),
        }
    })?;
    Ok(json_to_dynamic(result))
}

/// All known scripts as `Value::List` of `{id, metadata}` Maps.
/// Local-only.
pub fn scripts_list() -> Result<Value, Error> {
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::scripts_list: get_db: {e}")))?;
        let entries = db.scripts_with_metadata()
            .map_err(|e| err_msg(format!("vm::api::scripts_list: db: {e}")))?;
        let arr: Vec<serde_json::Value> = entries.into_iter().map(|(id, meta)| {
            json!({"id": id.to_string(), "metadata": meta})
        }).collect();
        Ok(serde_json::Value::Array(arr))
    })?;
    Ok(json_to_dynamic(result))
}
