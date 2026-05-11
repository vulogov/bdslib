//! Search surface: vector / FTS / aggregation / fulltext.
//!
//! Every helper here is a cluster-aware **read**.  Standalone mode runs
//! the local `ShardsManager` method; cluster mode also fans out the
//! matching v2/* method to peers and merges via `cluster::merge`.
//!
//! Cluster merge strategies:
//!
//! | Helper                 | Merge                                       |
//! |------------------------|---------------------------------------------|
//! | `search_vector`        | UUID dedup + score average                  |
//! | `search_get`           | UUID dedup + score average + truncate(limit)|
//! | `search_fts`           | UUID dedup (first-seen wins)                |
//! | `aggregation_search`   | Two arrays — both UUID dedup + score average|
//! | `fulltext`             | UUID dedup + score average + truncate(limit)|
//! | `fulltext_recent`      | UUID dedup, sort by timestamp DESC, truncate|
//! | `fulltext_get`         | UUID dedup (first-seen wins)                |

use crate::cluster::merge;
use crate::vm::api::dispatch;
use crate::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use easy_error::{err_msg, Error};
use rust_dynamic::value::Value;
use serde_json::json;

/// Vector search across the cluster.  `query` may be a string or a
/// structured Map; both go through the embedding layer the same way
/// the v2 method does.  Returns a `Value::List` of doc Maps.
pub fn search_vector(duration: &str, query: Value) -> Result<Value, Error> {
    let q_json = dynamic_to_json(query);
    let merged = dispatch::read(
        "v2/search.get",
        json!({"session": "", "duration": duration, "query": q_json, "limit": 1000}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::search_vector: get_db: {e}")))?;
            let hits = db.search_vector(duration, &q_json)
                .map_err(|e| err_msg(format!("vm::api::search_vector: db: {e}")))?;
            Ok(json!({"results": hits}))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            json!({"results": merge::dedup_avg_score(bodies, "results")})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Vector search returning **full documents** (companion to
/// `search_vector` which is `vectorsearch_recent` underneath).  This
/// matches the `v2/search.get` shape with `limit`.
pub fn search_get(duration: &str, query: Value, limit: usize) -> Result<Value, Error> {
    let q_json = dynamic_to_json(query);
    let merged = dispatch::read(
        "v2/search.get",
        json!({"session": "", "query": q_json, "duration": duration, "limit": limit}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::search_get: get_db: {e}")))?;
            let docs = db.vectorsearch_recent(duration, &q_json, limit)
                .map_err(|e| err_msg(format!("vm::api::search_get: db: {e}")))?;
            Ok(json!({"results": docs}))
        },
        move |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            let mut merged = merge::dedup_avg_score(bodies, "results");
            merged.truncate(limit);
            json!({"results": merged})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Full-text search returning the matching documents (no scores).
/// Matches `v2/search` (FTS) shape.
pub fn search_fts(duration: &str, query: &str) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/search",
        json!({"session": "", "duration": duration, "query": query}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::search_fts: get_db: {e}")))?;
            let docs = db.search_fts(duration, query)
                .map_err(|e| err_msg(format!("vm::api::search_fts: db: {e}")))?;
            Ok(json!({"results": docs}))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            json!({"results": merge::dedup_by_id(bodies, "results")})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Combined observability vector search + document-store semantic search.
/// Returns a Map `{observability: [...], documents: [...]}` — the same
/// shape `v2/aggregationsearch` produces.
pub fn aggregation_search(duration: &str, query: &str) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/aggregationsearch",
        json!({"session": "", "duration": duration, "query": query}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::aggregation_search: get_db: {e}")))?;
            db.aggregationsearch(duration, query)
                .map_err(|e| err_msg(format!("vm::api::aggregation_search: db: {e}")))
        },
        |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            json!({
                "observability": merge::dedup_avg_score(bodies.clone(), "observability"),
                "documents":     merge::dedup_avg_score(bodies,         "documents"),
            })
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// BM25 full-text search returning `(id, score)` pairs.  Cluster
/// merge: UUID dedup + score average + truncate(limit).
pub fn fulltext(duration: &str, query: &str, limit: usize) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/fulltext",
        json!({"session": "", "query": query, "duration": duration, "limit": limit}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::fulltext: get_db: {e}")))?;
            let hits = db.fulltextsearch(duration, query, limit)
                .map_err(|e| err_msg(format!("vm::api::fulltext: db: {e}")))?;
            let arr: Vec<serde_json::Value> = hits.into_iter()
                .map(|(id, score)| json!({"id": id.to_string(), "score": score}))
                .collect();
            Ok(json!({"results": arr}))
        },
        move |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            let mut merged = merge::dedup_avg_score(bodies, "results");
            merged.truncate(limit);
            json!({"results": merged})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// BM25 full-text "recent" — same as `fulltext` but each hit also
/// carries a `timestamp` and the merge sorts newest-first.
pub fn fulltext_recent(duration: &str, query: &str, limit: usize) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/fulltext.recent",
        json!({"session": "", "query": query, "duration": duration, "limit": limit}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::fulltext_recent: get_db: {e}")))?;
            let hits = db.fulltextsearch_recent(duration, query, limit)
                .map_err(|e| err_msg(format!("vm::api::fulltext_recent: db: {e}")))?;
            let arr: Vec<serde_json::Value> = hits.into_iter()
                .map(|(id, ts, score)| json!({"id": id.to_string(), "timestamp": ts, "score": score}))
                .collect();
            Ok(json!({"results": arr}))
        },
        move |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            let mut merged = merge::dedup_by_id_newest_first(bodies, "results");
            merged.truncate(limit);
            json!({"results": merged})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// BM25 full-text returning full documents (companion to `fulltext`).
/// Cluster merge: UUID dedup, first-seen wins.
pub fn fulltext_get(duration: &str, query: &str, limit: usize) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/fulltext.get",
        json!({"session": "", "query": query, "duration": duration, "limit": limit}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::fulltext_get: get_db: {e}")))?;
            let docs = db.search_fts(duration, query)
                .map_err(|e| err_msg(format!("vm::api::fulltext_get: db: {e}")))?;
            Ok(json!({"results": docs}))
        },
        move |local, fan| {
            let bodies = merge::bodies_from(&local, fan);
            let mut merged = merge::dedup_by_id(bodies, "results");
            merged.truncate(limit);
            json!({"results": merged})
        },
    )?;
    Ok(json_to_dynamic(merged))
}
