//! Reusable merge primitives for v3/* fan-out reads.
//!
//! Most v3 read methods follow the same recipe:
//!
//! 1. Run the local v2/* handler (in-process, blocking thread).
//! 2. Fan out the same v2/* method to every Alive peer.
//! 3. Merge the responses with a per-method strategy (UUID dedup, set
//!    union, score average, …).
//! 4. Truncate / sort and return.
//!
//! This module factors out the merge step.  Each helper takes an
//! iterator of "results" arrays (one per peer + the local one) and
//! collapses them into a single `JsonValue` array under whatever
//! semantics fit the method.

use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Iterate over `results[]` arrays from a sequence of v2-shaped responses.
/// Each response may be either:
///   - an object with a `key`-named array field, e.g. `{"results": [...]}`, or
///   - a bare JSON array (e.g. `v2/primaries.explore`).
///
/// Materialises the result into a `Vec` so borrow lifetimes stay simple —
/// per-call sizes are small (≤ a few thousand items) so this isn't a
/// hot-path concern.
pub fn extract_arrays<'a>(bodies: impl IntoIterator<Item = &'a JsonValue>, key: &str) -> Vec<&'a JsonValue> {
    let mut out: Vec<&'a JsonValue> = Vec::new();
    for b in bodies {
        if let Some(arr) = b.as_array() {
            out.extend(arr.iter());
        } else if let Some(arr) = b.get(key).and_then(|v| v.as_array()) {
            out.extend(arr.iter());
        }
    }
    out
}

/// UUID dedup, **first-seen wins**.  Each item in the per-peer arrays
/// must carry an `"id"` field.  Items with no `id` are skipped.  Final
/// order is the order of first appearance across the (local-first) input.
pub fn dedup_by_id(bodies: Vec<&JsonValue>, key: &str) -> Vec<JsonValue> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out:  Vec<JsonValue>  = Vec::new();
    for item in extract_arrays(bodies, key) {
        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() { continue; }
        if seen.insert(id.to_owned()) {
            out.push(item.clone());
        }
    }
    out
}

/// UUID dedup with **score averaging**: when the same id appears on
/// multiple peers the per-peer scores are averaged.  Final order: by
/// averaged score, descending.
pub fn dedup_avg_score(bodies: Vec<&JsonValue>, key: &str) -> Vec<JsonValue> {
    // (sum_score, n_seen, first_item_clone)
    let mut acc: HashMap<String, (f64, u64, JsonValue)> = HashMap::new();
    for item in extract_arrays(bodies, key) {
        let id = match item.get("id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_owned(),
            _ => continue,
        };
        let sc = item.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        match acc.get_mut(&id) {
            Some(e) => { e.0 += sc; e.1 += 1; }
            None    => { acc.insert(id, (sc, 1, item.clone())); }
        }
    }
    let mut rows: Vec<JsonValue> = acc.into_values().map(|(sum, n, mut item)| {
        let avg = if n > 0 { sum / n as f64 } else { 0.0 };
        if let Some(obj) = item.as_object_mut() {
            obj.insert("score".into(),    serde_json::json!(avg));
            obj.insert("replicas".into(), serde_json::json!(n));
        }
        item
    }).collect();
    rows.sort_by(|a, b| {
        let sa = a.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let sb = b.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

/// UUID dedup, sort by `timestamp` **descending** (most-recent first).
/// Used by `v3/fulltext.recent` and similar "newest first" paths.
pub fn dedup_by_id_newest_first(bodies: Vec<&JsonValue>, key: &str) -> Vec<JsonValue> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut rows: Vec<JsonValue>  = Vec::new();
    for item in extract_arrays(bodies, key) {
        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() { continue; }
        if seen.insert(id.to_owned()) {
            rows.push(item.clone());
        }
    }
    rows.sort_by(|a, b| {
        let ta = a.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
        let tb = b.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
        tb.cmp(&ta)
    });
    rows
}

/// Sorted union of strings from each response's `key`-named array.
/// Used by `v3/keys`, `v3/keys.all`.
pub fn union_strings(bodies: Vec<&JsonValue>, key: &str) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for body in &bodies {
        if let Some(arr) = body.get(key).and_then(|v| v.as_array()) {
            for v in arr {
                if let Some(s) = v.as_str() {
                    set.insert(s.to_owned());
                }
            }
        }
    }
    set.into_iter().collect()
}

/// Merge `[{key, count, …}]` arrays by summing per-key counts (with
/// UUID-set union for any `primaries`/`secondaries` arrays).
///
/// Returns rows shaped like the input but with merged counts and
/// deduplicated UUID arrays, sorted by `count` descending.
#[allow(dead_code)]
pub fn merge_key_count_rows(bodies: Vec<&JsonValue>, key_field: &str) -> Vec<JsonValue> {
    // For each row key: track count + per-uuid-array deduped sets.
    struct Acc {
        key:        String,
        count:      u64,
        primaries:  BTreeSet<String>,
        secondaries: BTreeSet<String>,
        extra:      Option<JsonValue>,  // fall-through fields (e.g. data_text)
    }
    let mut by_key: BTreeMap<String, Acc> = BTreeMap::new();

    for item in extract_arrays(bodies, "results") {
        let k = match item.get(key_field).and_then(|v| v.as_str()) {
            Some(s) => s.to_owned(),
            None    => continue,
        };
        let entry = by_key.entry(k.clone()).or_insert_with(|| Acc {
            key: k,
            count: 0,
            primaries: BTreeSet::new(),
            secondaries: BTreeSet::new(),
            extra: None,
        });
        entry.count = entry.count.saturating_add(item.get("count").and_then(|v| v.as_u64()).unwrap_or(0));
        for u in item.get("primaries").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(s) = u.as_str() { entry.primaries.insert(s.to_owned()); }
        }
        for u in item.get("secondaries").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(s) = u.as_str() { entry.secondaries.insert(s.to_owned()); }
        }
        if entry.extra.is_none() {
            entry.extra = Some(item.clone());
        }
    }

    let mut rows: Vec<JsonValue> = by_key.into_values().map(|a| {
        let mut obj = serde_json::Map::new();
        obj.insert(key_field.to_owned(), JsonValue::String(a.key));
        obj.insert("count".to_owned(),   JsonValue::from(a.count));
        if !a.primaries.is_empty() {
            obj.insert("primaries".to_owned(),
                JsonValue::Array(a.primaries.into_iter().map(JsonValue::String).collect()));
        }
        if !a.secondaries.is_empty() {
            obj.insert("secondaries".to_owned(),
                JsonValue::Array(a.secondaries.into_iter().map(JsonValue::String).collect()));
        }
        JsonValue::Object(obj)
    }).collect();

    rows.sort_by(|a, b| {
        let ca = a.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        let cb = b.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        cb.cmp(&ca)
    });
    rows
}

/// Concatenate items from each response's `key`-named array, preserving
/// per-peer order.  No deduplication.  Reserved for future use.
#[allow(dead_code)]
pub fn concat_arrays(bodies: Vec<&JsonValue>, key: &str) -> Vec<JsonValue> {
    let mut out = Vec::new();
    for item in extract_arrays(bodies, key) {
        out.push(item.clone());
    }
    out
}
