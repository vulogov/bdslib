//! Fully-replicated document store: add / update / delete / get /
//! search / reindex / sync.
//!
//! Writes go local-first then replicate to **every** Alive peer using
//! the same `id` so anti-entropy / hint replay can converge cleanly.
//! Reads are local — anti-entropy keeps every node's docstore aligned,
//! and the v2/doc.search* methods don't have v3 fan-out variants.

use crate::vm::api::dispatch;
use crate::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use easy_error::{err_msg, Error};
use rust_dynamic::value::Value;
use serde_json::json;
use uuid::Uuid;

/// Add a new document with `metadata` (JSON Map) and `content` (bytes).
/// Returns the new document UUID as a `Value::String`.
pub fn doc_add(metadata: Value, content: Vec<u8>) -> Result<Value, Error> {
    let meta_json = dynamic_to_json(metadata);
    let id_str = dispatch::write_replicated(
        "v2/doc.add",
        json!({"metadata": meta_json.clone(), "content": String::from_utf8_lossy(&content).into_owned()}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::doc_add: get_db: {e}")))?;
            let id = db.doc_add(meta_json.clone(), &content)
                .map_err(|e| err_msg(format!("vm::api::doc_add: db: {e}")))?;
            Ok(id.to_string())
        },
        |id, params| {
            // Inject the canonical id so replicas write under it.
            if let Some(obj) = params.as_object_mut() {
                obj.insert("id".into(), json!(id));
            }
        },
    )?;
    Ok(Value::from_string(id_str))
}

/// Read content from a file path and add it as a document.  The
/// `metadata` Map should contain at least a `name` field; the helper
/// adds `original_filename` automatically.  Local-only (no v3 fan-out
/// — peers see the document via the regular replicated `doc_add` after
/// the local file read).
pub fn doc_add_file(metadata: Value, path: &str) -> Result<Value, Error> {
    let content = std::fs::read(path)
        .map_err(|e| err_msg(format!("vm::api::doc_add_file: read {path}: {e}")))?;
    let mut meta_json = dynamic_to_json(metadata);
    if let Some(obj) = meta_json.as_object_mut() {
        obj.entry("original_filename".to_string())
           .or_insert_with(|| json!(path));
    }
    doc_add(json_to_dynamic(meta_json), content)
}

/// Replace a document's metadata.
pub fn doc_update_metadata(id: Value, metadata: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::doc_update_metadata: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::doc_update_metadata: parse id: {e}")))?;
    let meta_json = dynamic_to_json(metadata);
    let _ = dispatch::write_replicated(
        "v2/doc.update.metadata",
        json!({"id": id_str, "metadata": meta_json.clone()}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::doc_update_metadata: get_db: {e}")))?;
            db.doc_update_metadata(uuid, meta_json.clone())
                .map_err(|e| err_msg(format!("vm::api::doc_update_metadata: db: {e}")))?;
            Ok(id_str.clone())
        },
        |_, _| {},
    )?;
    Ok(Value::nodata())
}

/// Replace a document's content.
pub fn doc_update_content(id: Value, content: Vec<u8>) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::doc_update_content: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::doc_update_content: parse id: {e}")))?;
    let _ = dispatch::write_replicated(
        "v2/doc.update.content",
        json!({"id": id_str, "content": String::from_utf8_lossy(&content).into_owned()}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::doc_update_content: get_db: {e}")))?;
            db.doc_update_content(uuid, &content)
                .map_err(|e| err_msg(format!("vm::api::doc_update_content: db: {e}")))?;
            Ok(id_str.clone())
        },
        |_, _| {},
    )?;
    Ok(Value::nodata())
}

/// Delete a document by id.
pub fn doc_delete(id: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::doc_delete: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::doc_delete: parse id: {e}")))?;
    let _ = dispatch::write_replicated(
        "v2/doc.delete",
        json!({"id": id_str}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::doc_delete: get_db: {e}")))?;
            db.doc_delete(uuid)
                .map_err(|e| err_msg(format!("vm::api::doc_delete: db: {e}")))?;
            Ok(id_str.clone())
        },
        |_, _| {},
    )?;
    Ok(Value::nodata())
}

/// Fetch a document's metadata Map.  Local-only.
pub fn doc_get_metadata(id: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::doc_get_metadata: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::doc_get_metadata: parse id: {e}")))?;
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::doc_get_metadata: get_db: {e}")))?;
        Ok(db.doc_get_metadata(uuid)
            .map_err(|e| err_msg(format!("vm::api::doc_get_metadata: db: {e}")))?
            .unwrap_or(serde_json::Value::Null))
    })?;
    Ok(json_to_dynamic(result))
}

/// Fetch a document's content as bytes (Value::Binary).  Local-only.
pub fn doc_get_content(id: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::doc_get_content: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::doc_get_content: parse id: {e}")))?;
    let bytes = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::doc_get_content: get_db: {e}")))?;
        let opt = db.doc_get_content(uuid)
            .map_err(|e| err_msg(format!("vm::api::doc_get_content: db: {e}")))?;
        Ok(opt.unwrap_or_default())
    })?;
    Ok(Value::from_bin(bytes))
}

/// Vector search across the local docstore.  `query` may be a plain
/// string (embedded server-side) or a structured Map.  Local-only.
pub fn doc_search(query: Value, limit: usize) -> Result<Value, Error> {
    let q_json = dynamic_to_json(query);
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::doc_search: get_db: {e}")))?;
        let docs = match q_json {
            serde_json::Value::String(s) => db.doc_search_text(&s, limit),
            ref other                    => db.doc_search_json(other, limit),
        }.map_err(|e| err_msg(format!("vm::api::doc_search: db: {e}")))?;
        Ok(json!({"results": docs}))
    })?;
    Ok(json_to_dynamic(result))
}

/// `doc_search` returning content strings only (no metadata).
pub fn doc_search_strings(query: Value, limit: usize) -> Result<Value, Error> {
    let q_json = dynamic_to_json(query);
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::doc_search_strings: get_db: {e}")))?;
        let docs = match q_json {
            serde_json::Value::String(s) => db.doc_search_text_strings(&s, limit),
            ref other                    => db.doc_search_json_strings(other, limit),
        }.map_err(|e| err_msg(format!("vm::api::doc_search_strings: db: {e}")))?;
        Ok(json!({"results": docs}))
    })?;
    Ok(json_to_dynamic(result))
}

/// JSON-shaped vector search (alternate input form).
pub fn doc_search_json(query: Value, limit: usize) -> Result<Value, Error> {
    let q_json = dynamic_to_json(query);
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::doc_search_json: get_db: {e}")))?;
        let docs = db.doc_search_json(&q_json, limit)
            .map_err(|e| err_msg(format!("vm::api::doc_search_json: db: {e}")))?;
        Ok(json!({"results": docs}))
    })?;
    Ok(json_to_dynamic(result))
}

/// `doc_search_json` returning content strings only.
pub fn doc_search_json_strings(query: Value, limit: usize) -> Result<Value, Error> {
    let q_json = dynamic_to_json(query);
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::doc_search_json_strings: get_db: {e}")))?;
        let docs = db.doc_search_json_strings(&q_json, limit)
            .map_err(|e| err_msg(format!("vm::api::doc_search_json_strings: db: {e}")))?;
        Ok(json!({"results": docs}))
    })?;
    Ok(json_to_dynamic(result))
}

/// Re-embed every docstore record.  Returns the Value::Int count
/// of records re-embedded.  Local-only.
pub fn doc_reindex() -> Result<Value, Error> {
    let n = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::doc_reindex: get_db: {e}")))?;
        db.doc_reindex()
            .map_err(|e| err_msg(format!("vm::api::doc_reindex: db: {e}")))
    })?;
    Ok(Value::from_int(n as i64))
}

/// Flush any pending docstore writes (CHECKPOINT).  Local-only.
pub fn doc_sync() -> Result<Value, Error> {
    dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::doc_sync: get_db: {e}")))?;
        db.doc_sync()
            .map_err(|e| err_msg(format!("vm::api::doc_sync: db: {e}")))
    })?;
    Ok(Value::nodata())
}
