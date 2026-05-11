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
//! This module factors out the merge step so the bdsnode v3_*.rs
//! handlers and the `vm::api::*` cluster-aware helpers share the same
//! code.  Two flavours of helper live here:
//!
//! - **Body-array helpers** (`extract_arrays`, `dedup_*`, `union_*`,
//!   `concat_*`) — operate on a flat `Vec<&JsonValue>` of per-peer
//!   bodies.  Callers build the slice as `[&local, peer1, peer2, …]`
//!   before calling.
//! - **(local, fan) helpers** (`pick_largest_by_field`,
//!   `pick_longest_string`, `min_max_fields`, `sum_field`, …) —
//!   take the local body plus the optional `FanOutResults` directly so
//!   call sites stay one-liners.  These match the most common
//!   "pick / aggregate" merges across handlers.

use crate::cluster::fanout::FanOutResults;
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

// ─────────────────────────────────────────────────────────────────────────────
// Body-array helpers — operate on `Vec<&JsonValue>`.
// ─────────────────────────────────────────────────────────────────────────────

/// Iterate over `key`-named arrays from a sequence of v2-shaped responses.
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
///
/// Each output item gains a `replicas: <count>` field for callers that
/// want to know how many peers held a copy.
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

// ─────────────────────────────────────────────────────────────────────────────
// (local, fan) helpers — take the local body + optional FanOutResults.
// Reduces every "pick the best response" handler to a one-liner.
// ─────────────────────────────────────────────────────────────────────────────

/// Iterate over `local` followed by every successful peer response.
/// Returned vec lifetimes tie to the inputs so callers can pass it to
/// any of the body-array helpers above.
pub fn bodies_from<'a>(
    local: &'a JsonValue,
    fan:   Option<&'a FanOutResults>,
) -> Vec<&'a JsonValue> {
    let mut out: Vec<&'a JsonValue> = vec![local];
    if let Some(f) = fan {
        out.extend(f.ok_results());
    }
    out
}

/// Pick the response (`local` or any peer) whose top-level `field` holds
/// the largest non-negative integer.  Returns `(chosen_clone, best_value)`.
/// Used by v3/topics, v3/rca, v3/trends — all "pick richest corpus" merges.
pub fn pick_largest_by_field(
    local: &JsonValue,
    fan:   Option<&FanOutResults>,
    field: &str,
) -> (JsonValue, u64) {
    let mut best: &JsonValue = local;
    let mut best_n           = local.get(field).and_then(|v| v.as_u64()).unwrap_or(0);
    if let Some(f) = fan {
        for r in f.ok_results() {
            let n = r.get(field).and_then(|v| v.as_u64()).unwrap_or(0);
            if n > best_n { best = r; best_n = n; }
        }
    }
    (best.clone(), best_n)
}

/// Pick the response whose top-level `field` (a string) is longest.
/// Used by v3/textrank.templates, v3/summary_for_*, v3/summary_lsa_for_*.
pub fn pick_longest_string(
    local: &JsonValue,
    fan:   Option<&FanOutResults>,
    field: &str,
) -> JsonValue {
    let str_len = |v: &JsonValue| v.get(field).and_then(|s| s.as_str()).map(str::len).unwrap_or(0);
    let mut best: &JsonValue = local;
    let mut best_len         = str_len(local);
    if let Some(f) = fan {
        for r in f.ok_results() {
            let n = str_len(r);
            if n > best_len { best = r; best_len = n; }
        }
    }
    best.clone()
}

/// Min over `min_field` and max over `max_field` across local + peers.
/// `None` is propagated as "no value seen".  Used by v3/timeline.
pub fn min_max_fields(
    local:     &JsonValue,
    fan:       Option<&FanOutResults>,
    min_field: &str,
    max_field: &str,
) -> (Option<i64>, Option<i64>) {
    let mut min_v = local.get(min_field).and_then(|v| v.as_i64());
    let mut max_v = local.get(max_field).and_then(|v| v.as_i64());
    if let Some(f) = fan {
        for r in f.ok_results() {
            let m = r.get(min_field).and_then(|v| v.as_i64());
            let x = r.get(max_field).and_then(|v| v.as_i64());
            min_v = match (min_v, m) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (None,    b)       => b,
                (a,       None)    => a,
            };
            max_v = match (max_v, x) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (None,    b)       => b,
                (a,       None)    => a,
            };
        }
    }
    (min_v, max_v)
}

/// Sum the local value with every peer's `field` (interpreted as `u64`).
/// Used by v3/count (default sum mode).
pub fn sum_field(local_value: u64, fan: Option<&FanOutResults>, field: &str) -> u64 {
    let mut total = local_value;
    if let Some(f) = fan {
        for r in f.ok_results() {
            total = total.saturating_add(r.get(field).and_then(|v| v.as_u64()).unwrap_or(0));
        }
    }
    total
}

/// Distinct (set-union) count of string IDs from each response's `field`-named
/// array.  Used by v3/count distinct mode and v3/primaries (sorted union).
/// Returns `(union_size, sorted_vec)` so callers can use either.
pub fn union_string_ids(
    local: &JsonValue,
    fan:   Option<&FanOutResults>,
    field: &str,
) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    if let Some(arr) = local.get(field).and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() { set.insert(s.to_owned()); }
        }
    }
    if let Some(f) = fan {
        for r in f.ok_results() {
            if let Some(arr) = r.get(field).and_then(|v| v.as_array()) {
                for v in arr {
                    if let Some(s) = v.as_str() { set.insert(s.to_owned()); }
                }
            }
        }
    }
    set.into_iter().collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Specialised mergers — too custom to express as primitives.
// ─────────────────────────────────────────────────────────────────────────────

/// Per-key sum of `count`, union of `primary_id[]`.  Returns rows shaped
/// `{key, count, raw_count, primary_id}` sorted by deduped `count`
/// descending.
///
/// Used by v3/primaries.explore and v3/primaries.explore.telemetry —
/// both produce bare arrays per peer.  `count` in the result is the
/// **deduplicated** count (size of UUID set); `raw_count` preserves the
/// sum of per-peer counts so callers can see the duplication factor.
pub fn merge_explore_rows(bodies: Vec<&JsonValue>) -> Vec<JsonValue> {
    struct Acc { count: u64, ids: BTreeSet<String> }
    let mut by_key: BTreeMap<String, Acc> = BTreeMap::new();
    for item in extract_arrays(bodies, "results") {
        let key = match item.get("key").and_then(|v| v.as_str()) {
            Some(s) => s.to_owned(),
            None    => continue,
        };
        let entry = by_key.entry(key).or_insert_with(|| Acc { count: 0, ids: BTreeSet::new() });
        entry.count = entry.count.saturating_add(
            item.get("count").and_then(|v| v.as_u64()).unwrap_or(0));
        for u in item.get("primary_id").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(s) = u.as_str() { entry.ids.insert(s.to_owned()); }
        }
    }

    let mut items: Vec<JsonValue> = by_key.into_iter().map(|(key, a)| {
        serde_json::json!({
            "key":        key,
            "count":      a.ids.len() as u64,
            "raw_count":  a.count,
            "primary_id": a.ids.into_iter().collect::<Vec<_>>(),
        })
    }).collect();
    items.sort_by(|a, b| {
        let ca = a.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        let cb = b.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        cb.cmp(&ca)
    });
    items
}

/// Merge per-`primary_id` rows: union `secondary_ids[]`, keep the first
/// `timestamp` seen for each primary.  Sort by `timestamp` descending.
/// Used by v3/keys.get.
pub fn merge_keys_get_rows(bodies: Vec<&JsonValue>) -> Vec<JsonValue> {
    let mut by_primary: HashMap<String, (i64, BTreeSet<String>)> = HashMap::new();
    for item in extract_arrays(bodies, "results") {
        let pid = match item.get("primary_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_owned(),
            None    => continue,
        };
        let ts = item.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
        let entry = by_primary.entry(pid).or_insert_with(|| (ts, BTreeSet::new()));
        for s in item.get("secondary_ids").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(s) = s.as_str() { entry.1.insert(s.to_owned()); }
        }
    }
    let mut rows: Vec<JsonValue> = by_primary.into_iter().map(|(pid, (ts, sids))| {
        serde_json::json!({
            "primary_id":    pid,
            "timestamp":     ts,
            "secondary_ids": sids.into_iter().collect::<Vec<_>>(),
        })
    }).collect();
    rows.sort_by(|a, b| {
        let ta = a.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
        let tb = b.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
        tb.cmp(&ta)
    });
    rows
}

/// Per-key, pick the entry whose `corpus_size_extractor` returns the
/// highest value across local + peers.  Output sorted by `key` ASC.
/// Used by v3/topics.all — each peer's `topics[]` may report a different
/// corpus size for the same key, and the largest-corpus version is the
/// most reliable summary.
pub fn pick_largest_per_key(
    local:                 &JsonValue,
    fan:                   Option<&FanOutResults>,
    items_field:           &str,
    key_field:             &str,
    corpus_size_extractor: fn(&JsonValue) -> u64,
) -> Vec<JsonValue> {
    let mut best_per_key: HashMap<String, (u64, JsonValue)> = HashMap::new();
    let bodies = bodies_from(local, fan);
    for body in &bodies {
        let arr = match body.get(items_field).and_then(|v| v.as_array()) {
            Some(a) => a,
            None    => continue,
        };
        for item in arr {
            let key = match item.get(key_field).and_then(|v| v.as_str()) {
                Some(s) => s.to_owned(),
                None    => continue,
            };
            let n = corpus_size_extractor(item);
            match best_per_key.get(&key) {
                Some((cur_n, _)) if n <= *cur_n => {}
                _ => { best_per_key.insert(key, (n, item.clone())); }
            }
        }
    }
    let mut out: Vec<JsonValue> = best_per_key.into_values().map(|(_, v)| v).collect();
    out.sort_by(|a, b| {
        let ka = a.get(key_field).and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get(key_field).and_then(|v| v.as_str()).unwrap_or("");
        ka.cmp(kb)
    });
    out
}

/// Best-effort "corpus size" extractor for LDA/RCA/topics responses.
/// Tries `n_records`, then `count`, then `total_records`.  Returns 0
/// when none of those fields exist or aren't unsigned integers.  Use
/// this with [`pick_largest_per_key`] (and it works as a callable for
/// `pick_largest_by_field`-style call sites too).
pub fn lda_corpus_size(v: &JsonValue) -> u64 {
    v.get("n_records").and_then(|x| x.as_u64())
        .or_else(|| v.get("count").and_then(|x| x.as_u64()))
        .or_else(|| v.get("total_records").and_then(|x| x.as_u64()))
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Legacy (unused-but-kept) — `merge_key_count_rows` was the original
// generic version of `merge_explore_rows` before `primary_id` /
// `secondaries` divergence forced a specialised version.  Kept under
// `#[allow(dead_code)]` so external code that may have started using it
// doesn't break.
// ─────────────────────────────────────────────────────────────────────────────

/// Merge `[{key, count, …}]` arrays by summing per-key counts (with
/// UUID-set union for any `primaries`/`secondaries` arrays).
///
/// Returns rows shaped like the input but with merged counts and
/// deduplicated UUID arrays, sorted by `count` descending.
#[allow(dead_code)]
pub fn merge_key_count_rows(bodies: Vec<&JsonValue>, key_field: &str) -> Vec<JsonValue> {
    struct Acc {
        key:         String,
        count:       u64,
        primaries:   BTreeSet<String>,
        secondaries: BTreeSet<String>,
        extra:       Option<JsonValue>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dedup_by_id_first_seen_wins() {
        let a = json!({"results": [{"id": "a", "v": 1}, {"id": "b", "v": 2}]});
        let b = json!({"results": [{"id": "a", "v": 99}, {"id": "c", "v": 3}]});
        let merged = dedup_by_id(vec![&a, &b], "results");
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0]["v"], 1, "first-seen wins for id=a");
    }

    #[test]
    fn dedup_avg_score_averages_duplicates() {
        let a = json!({"results": [{"id": "a", "score": 1.0}]});
        let b = json!({"results": [{"id": "a", "score": 3.0}]});
        let merged = dedup_avg_score(vec![&a, &b], "results");
        assert_eq!(merged[0]["score"].as_f64().unwrap(), 2.0);
        assert_eq!(merged[0]["replicas"].as_u64().unwrap(), 2);
    }

    #[test]
    fn pick_largest_by_field_picks_largest() {
        let local = json!({"n_events": 5, "tag": "local"});
        let (chosen, n) = pick_largest_by_field(&local, None, "n_events");
        assert_eq!(n, 5);
        assert_eq!(chosen["tag"], "local");
    }

    #[test]
    fn pick_longest_string_picks_longest() {
        let local = json!({"summary": "short"});
        // No fan in this tiny test — verifies the local-only path.
        let chosen = pick_longest_string(&local, None, "summary");
        assert_eq!(chosen["summary"], "short");
    }

    #[test]
    fn min_max_fields_returns_tightest_bounds() {
        let local = json!({"min_ts": 100, "max_ts": 200});
        let (min, max) = min_max_fields(&local, None, "min_ts", "max_ts");
        assert_eq!(min, Some(100));
        assert_eq!(max, Some(200));
    }

    #[test]
    fn sum_field_starts_from_local() {
        assert_eq!(sum_field(7, None, "count"), 7);
    }

    #[test]
    fn union_string_ids_dedups_and_sorts() {
        let local = json!({"ids": ["b", "a"]});
        let out = union_string_ids(&local, None, "ids");
        assert_eq!(out, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn merge_explore_rows_dedups_and_sums() {
        let body = json!([
            {"key": "k1", "count": 3, "primary_id": ["u1", "u2"]},
            {"key": "k1", "count": 2, "primary_id": ["u2", "u3"]},
        ]);
        let merged = merge_explore_rows(vec![&body]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["count"].as_u64().unwrap(), 3,
            "deduped UUID count for k1: {{u1,u2,u3}} = 3");
        assert_eq!(merged[0]["raw_count"].as_u64().unwrap(), 5,
            "raw sum of per-peer counts: 3 + 2 = 5");
    }
}
