//! Fully-replicated signals store: emit / update / get / recent / query.
//!
//! | Helper           | Cluster strategy                                              |
//! |------------------|---------------------------------------------------------------|
//! | `signal_emit`    | local + replicate to every Alive peer (with id injected)      |
//! | `signal_update`  | same                                                          |
//! | `signal_get`     | local-only (anti-entropy keeps replicas converged)            |
//! | `signals_recent` | read: UUID dedup (first-seen wins)                            |
//! | `signals_query`  | read: UUID dedup + score average + truncate(limit)            |

use crate::cluster::merge;
use crate::vm::api::dispatch;
use crate::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use easy_error::{err_msg, Error};
use rust_dynamic::value::Value;
use serde_json::json;
use uuid::Uuid;

/// Emit a new signal.  Returns the signal UUID as a `Value::String`.
/// In cluster mode the signal also lands on every Alive peer under the
/// same UUID (so anti-entropy doesn't end up replicating dupes).
///
/// `extra` is an optional Map of additional metadata fields merged into
/// the stored signal alongside `name` / `severity` / `timestamp`.  Pass
/// `Value::nodata()` for none.
pub fn signal_emit(name: &str, severity: &str, timestamp: u64, extra: Value) -> Result<Value, Error> {
    let extra_map = match dynamic_to_json(extra) {
        serde_json::Value::Object(m) => m,
        serde_json::Value::Null      => serde_json::Map::new(),
        other => {
            return Err(err_msg(format!(
                "vm::api::signal_emit: extra must be a Map or nodata, got {other:?}"
            )));
        }
    };

    // Local commit first to mint the canonical UUID.
    let db = crate::globals::get_db()
        .map_err(|e| err_msg(format!("vm::api::signal_emit: get_db: {e}")))?;
    let local_id = db.signal_emit(name, severity, timestamp, extra_map.clone())
        .map_err(|e| err_msg(format!("vm::api::signal_emit: db: {e}")))?;

    // Cluster fan-out under the same UUID via the v2/signal.emit `id`
    // override (peers use signal_emit_with_id when `id` is supplied).
    if let Some(cluster) = db.cluster() {
        let cluster = cluster.clone();
        let params = json!({
            "session": "",
            "name": name, "severity": severity, "timestamp": timestamp,
            "metadata": extra_map,
            "id": local_id.to_string(),
        });
        let outcome = crate::vm::api::runtime::block_on(
            crate::cluster::replication::replicate_to_all(cluster, "v2/signal.emit", params),
        );
        crate::vm::api::meta::set(json!({
            "enabled":     true,
            "replication": outcome.to_json(),
        }));
    } else {
        crate::vm::api::meta::clear();
    }
    Ok(Value::from_string(local_id.to_string()))
}

/// Replace a signal's metadata.  Replicates to every Alive peer.
pub fn signal_update(id: Value, metadata: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::signal_update: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::signal_update: parse id: {e}")))?;
    let meta_json = dynamic_to_json(metadata);
    let _ = dispatch::write_replicated(
        "v2/signal.update",
        json!({"id": id_str, "metadata": meta_json.clone()}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::signal_update: get_db: {e}")))?;
            db.signal_update(uuid, meta_json.clone())
                .map_err(|e| err_msg(format!("vm::api::signal_update: db: {e}")))?;
            Ok(id_str.clone())
        },
        |_, _| {},
    )?;
    Ok(Value::nodata())
}

/// Fetch a signal's metadata by id.  Local-only — anti-entropy keeps
/// every node's signal store converged so the local read is authoritative.
pub fn signal_get(id: Value) -> Result<Value, Error> {
    let id_str = id.cast_string()
        .map_err(|e| err_msg(format!("vm::api::signal_get: id cast: {e}")))?;
    let uuid = Uuid::parse_str(&id_str)
        .map_err(|e| err_msg(format!("vm::api::signal_get: parse id: {e}")))?;
    let result = dispatch::write_local(|| {
        let db = crate::globals::get_db()
            .map_err(|e| err_msg(format!("vm::api::signal_get: get_db: {e}")))?;
        let meta = db.signal_get(uuid)
            .map_err(|e| err_msg(format!("vm::api::signal_get: db: {e}")))?;
        Ok(meta.unwrap_or(serde_json::Value::Null))
    })?;
    Ok(json_to_dynamic(result))
}

/// Signal IDs (with metadata) emitted in the trailing `duration` window.
/// Cluster merge: UUID dedup (first-seen wins).
pub fn signals_recent(duration: &str) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/signals",
        json!({"session": "", "duration": duration}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::signals_recent: get_db: {e}")))?;
            let ids = db.signals_recent(duration)
                .map_err(|e| err_msg(format!("vm::api::signals_recent: db: {e}")))?;
            let signals: Vec<serde_json::Value> = ids.iter().map(|id_str| {
                let meta = Uuid::parse_str(id_str)
                    .ok()
                    .and_then(|u| db.signal_get(u).ok().flatten())
                    .unwrap_or(serde_json::Value::Null);
                json!({"id": id_str, "metadata": meta})
            }).collect();
            Ok(json!({"duration": duration, "count": signals.len(), "signals": signals}))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            let signals = merge::dedup_by_id(bodies, "signals");
            json!({"duration": duration, "count": signals.len(), "signals": signals})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Semantic search over signal metadata.
pub fn signals_query(query: &str, limit: usize) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/signals_query",
        json!({"session": "", "query": query, "limit": limit}),
        move || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::signals_query: get_db: {e}")))?;
            let results = db.signals_query(query, limit)
                .map_err(|e| err_msg(format!("vm::api::signals_query: db: {e}")))?;
            Ok(json!({"query": query, "count": results.len(), "results": results}))
        },
        move |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            let mut results = merge::dedup_avg_score(bodies, "results");
            results.truncate(limit);
            json!({"query": query, "count": results.len(), "results": results})
        },
    )?;
    Ok(json_to_dynamic(merged))
}
