//! RAG context pipeline for `v4/llm.analyze`.
//!
//! [`ContextSource`] is the discriminated input — one variant per kind
//! of bdslib data the operator wants the model to see.  [`build`] runs
//! the matching `vm::api::*` helper (cluster-aware on multi-node
//! deployments, local-only standalone), extracts the relevant rows,
//! json-fingerprints each row, and packs the result into [`RagContext`]
//! ready to splice into a prompt.
//!
//! ```text
//! ContextSource::Rca {...} ──► vm::api::analysis::rca ──► rows
//!                                                          │
//!                                          json_fingerprint each row
//!                                                          │
//!                                                          ▼
//!                                                  RagContext { summary, rows, … }
//! ```
//!
//! Caller never sees an `is_cluster_call` branch — that decision happens
//! inside the dispatch layer.

use crate::common::error::{err_msg, Result};
use crate::common::jsonfingerprint::json_fingerprint;
use crate::globals::get_db;
use crate::vm::api;
use crate::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use serde_json::{json, Value as JsonValue};
use uuid::Uuid;

/// Limits applied per-kind so a single bad call can't blow the prompt
/// token budget.  Adjustable per `ContextSource` via the `limit` field.
const DEFAULT_LIMIT: usize = 30;

/// One row of context as bdslib produced it, plus the flattened
/// fingerprint string fed into the prompt.
#[derive(Debug, Clone)]
pub struct ContextRow {
    pub raw:         JsonValue,
    pub fingerprint: String,
}

/// Outcome of one [`build`] call.  `summary` is the prompt-ready joined
/// fingerprint string; `rows` is the raw underlying data so callers
/// that need to render or persist hit-level detail (bdsweb pages,
/// inference cache key) can do so without re-running the search.
#[derive(Debug, Clone)]
pub struct RagContext {
    pub rows:        Vec<ContextRow>,
    pub summary:     String,
    pub n_rows:      usize,
    /// Echoed back to the operator so they can see *what* was fetched.
    /// Shape:
    /// `{kind, duration?, query?, limit, telemetry_count?, document_count?, …}`.
    pub source_meta: JsonValue,
}

impl RagContext {
    pub fn empty(kind: &str) -> Self {
        Self {
            rows:        Vec::new(),
            summary:     String::new(),
            n_rows:      0,
            source_meta: json!({ "kind": kind }),
        }
    }
}

/// Discriminated source description.  Variants line up 1:1 with the
/// `kind` values accepted by `v4/llm.analyze`.
#[derive(Debug, Clone)]
pub enum ContextSource {
    /// `vm::api::search::aggregation_search` — observability rows + matched documents.
    Aggregation { duration: String, query: String, limit: Option<usize> },

    /// `vm::api::analysis::knn` — k-NN over recent fingerprints.
    Knn { duration: String, query: String, k: Option<usize> },

    /// `vm::api::analysis::rca` — root-cause candidates over event clusters.
    Rca {
        duration:          String,
        failure_key:       Option<String>,
        bucket_secs:       Option<u64>,
        min_support:       Option<u64>,
        jaccard_threshold: Option<f32>,
        max_keys:          Option<usize>,
    },

    /// `vm::api::analysis::anomaly_recent` — n-gram anomaly candidates.
    Anomaly { duration: String, limit: Option<usize> },

    /// `vm::api::analysis::textrank_templates` — TextRank-ranked drain templates.
    Templates { duration: String, top_n: Option<usize> },

    /// Top-N rows from `v2/fulltext.recent` — operator's recent activity feed.
    Telemetry { duration: String, query: Option<String>, limit: Option<usize> },

    /// Whole documents by id (metadata + content).
    Documents { ids: Vec<Uuid> },

    /// Caller-supplied verbatim rows ("data already on the page").
    Supplied { rows: Vec<JsonValue> },
}

impl ContextSource {
    pub fn kind(&self) -> &'static str {
        match self {
            ContextSource::Aggregation { .. } => "aggregation",
            ContextSource::Knn         { .. } => "knn",
            ContextSource::Rca         { .. } => "rca",
            ContextSource::Anomaly     { .. } => "anomaly",
            ContextSource::Templates   { .. } => "templates",
            ContextSource::Telemetry   { .. } => "telemetry",
            ContextSource::Documents   { .. } => "documents",
            ContextSource::Supplied    { .. } => "supplied",
        }
    }
}

/// Build a RAG context from `source`.  All helpers route through
/// `vm::api::*` so cluster fan-out / merge is transparent.
pub fn build(source: ContextSource) -> Result<RagContext> {
    match source {
        ContextSource::Aggregation { duration, query, limit } =>
            build_aggregation(&duration, &query, limit.unwrap_or(DEFAULT_LIMIT)),
        ContextSource::Knn { duration, query, k } =>
            build_knn(&duration, &query, k.unwrap_or(20)),
        ContextSource::Rca { duration, failure_key, bucket_secs, min_support, jaccard_threshold, max_keys } =>
            build_rca(&duration, failure_key, bucket_secs, min_support, jaccard_threshold, max_keys),
        ContextSource::Anomaly { duration, limit } =>
            build_anomaly(&duration, limit.unwrap_or(DEFAULT_LIMIT)),
        ContextSource::Templates { duration, top_n } =>
            build_templates(&duration, top_n.unwrap_or(20)),
        ContextSource::Telemetry { duration, query, limit } =>
            build_telemetry(&duration, query.as_deref().unwrap_or(""), limit.unwrap_or(DEFAULT_LIMIT)),
        ContextSource::Documents { ids } =>
            build_documents(&ids),
        ContextSource::Supplied { rows } =>
            Ok(build_supplied(rows)),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Per-variant builders
// ─────────────────────────────────────────────────────────────────────

fn build_aggregation(duration: &str, query: &str, limit: usize) -> Result<RagContext> {
    let val = api::search::aggregation_search(duration, query)
        .map_err(|e| err_msg(format!("context::aggregation: {e}")))?;
    let json = dynamic_to_json(val);

    let telemetry = json.get("observability").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let documents = json.get("documents").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let n_tel = telemetry.len();
    let n_doc = documents.len();

    // Mirror the chat helper's prompt segments so analyze-style and
    // chat-style RAG produce comparable strings:
    //   [telemetry N] <fp>
    //   [document  N] <fp>
    let mut rows: Vec<ContextRow> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    for (i, row) in telemetry.iter().take(limit).enumerate() {
        let fp = json_fingerprint(row);
        if fp.is_empty() { continue; }
        parts.push(format!("[telemetry {}] {}", i + 1, fp));
        rows.push(ContextRow { raw: row.clone(), fingerprint: fp });
    }
    for (i, row) in documents.iter().take((limit / 3).max(5)).enumerate() {
        let fp = json_fingerprint(row);
        if fp.is_empty() { continue; }
        parts.push(format!("[document {}] {}", i + 1, fp));
        rows.push(ContextRow { raw: row.clone(), fingerprint: fp });
    }

    Ok(RagContext {
        n_rows:  rows.len(),
        rows,
        summary: parts.join("\n"),
        source_meta: json!({
            "kind":            "aggregation",
            "duration":        duration,
            "query":           query,
            "limit":           limit,
            "telemetry_count": n_tel,
            "document_count":  n_doc,
        }),
    })
}

fn build_knn(duration: &str, query: &str, k: usize) -> Result<RagContext> {
    let opts = json_to_dynamic(json!({ "query": query, "k": k }));
    let val = api::analysis::knn(duration, opts)
        .map_err(|e| err_msg(format!("context::knn: {e}")))?;
    let json = dynamic_to_json(val);

    let rows_json = pick_rows_array(&json, &["candidates", "results", "items", "neighbors"]);
    let rows = fingerprint_rows(&rows_json, k);
    let summary = join_labelled("knn", &rows);

    Ok(RagContext {
        n_rows:  rows.len(),
        source_meta: json!({
            "kind":       "knn",
            "duration":   duration,
            "query":      query,
            "k":          k,
            "row_count":  rows.len(),
        }),
        rows,
        summary,
    })
}

fn build_rca(
    duration:          &str,
    failure_key:       Option<String>,
    bucket_secs:       Option<u64>,
    min_support:       Option<u64>,
    jaccard_threshold: Option<f32>,
    max_keys:          Option<usize>,
) -> Result<RagContext> {
    let mut opts = serde_json::Map::new();
    opts.insert("duration".into(), json!(duration));
    if let Some(k) = &failure_key       { opts.insert("failure_key".into(),       json!(k)); }
    if let Some(b) = bucket_secs        { opts.insert("bucket_secs".into(),       json!(b)); }
    if let Some(s) = min_support        { opts.insert("min_support".into(),       json!(s)); }
    if let Some(t) = jaccard_threshold  { opts.insert("jaccard_threshold".into(), json!(t)); }
    if let Some(m) = max_keys           { opts.insert("max_keys".into(),          json!(m)); }
    let opts_val = json_to_dynamic(JsonValue::Object(opts));

    let val = api::analysis::rca(opts_val)
        .map_err(|e| err_msg(format!("context::rca: {e}")))?;
    let json = dynamic_to_json(val);

    let rows_json = pick_rows_array(&json, &["candidates", "causes", "rca", "results"]);
    let n_events = json.get("n_events").and_then(|v| v.as_u64()).unwrap_or(0);
    let rows = fingerprint_rows(&rows_json, DEFAULT_LIMIT);
    let summary = join_labelled("cause", &rows);

    Ok(RagContext {
        n_rows: rows.len(),
        source_meta: json!({
            "kind":        "rca",
            "duration":    duration,
            "failure_key": failure_key,
            "n_events":    n_events,
            "row_count":   rows.len(),
        }),
        rows,
        summary,
    })
}

fn build_anomaly(duration: &str, limit: usize) -> Result<RagContext> {
    let val = api::analysis::anomaly_recent(duration, json_to_dynamic(json!({})))
        .map_err(|e| err_msg(format!("context::anomaly: {e}")))?;
    let json = dynamic_to_json(val);

    let rows_json = pick_rows_array(&json, &["anomalies", "results", "candidates", "items"]);
    let rows = fingerprint_rows(&rows_json, limit);
    let summary = join_labelled("anomaly", &rows);

    Ok(RagContext {
        n_rows: rows.len(),
        source_meta: json!({
            "kind":      "anomaly",
            "duration":  duration,
            "limit":     limit,
            "row_count": rows.len(),
        }),
        rows,
        summary,
    })
}

fn build_templates(duration: &str, top_n: usize) -> Result<RagContext> {
    let opts = json_to_dynamic(json!({ "top_n": top_n }));
    let val = api::analysis::textrank_templates(duration, opts)
        .map_err(|e| err_msg(format!("context::templates: {e}")))?;
    let json = dynamic_to_json(val);

    let rows_json = pick_rows_array(&json, &["templates", "results", "items"]);
    let rows = fingerprint_rows(&rows_json, top_n);
    let summary = join_labelled("template", &rows);

    Ok(RagContext {
        n_rows: rows.len(),
        source_meta: json!({
            "kind":      "templates",
            "duration":  duration,
            "top_n":     top_n,
            "row_count": rows.len(),
        }),
        rows,
        summary,
    })
}

fn build_telemetry(duration: &str, query: &str, limit: usize) -> Result<RagContext> {
    let val = api::search::fulltext_recent(duration, query, limit)
        .map_err(|e| err_msg(format!("context::telemetry: {e}")))?;
    let json = dynamic_to_json(val);

    let rows_json = pick_rows_array(&json, &["results", "hits", "items"]);
    let rows = fingerprint_rows(&rows_json, limit);
    let summary = join_labelled("telemetry", &rows);

    Ok(RagContext {
        n_rows: rows.len(),
        source_meta: json!({
            "kind":      "telemetry",
            "duration":  duration,
            "query":     query,
            "limit":     limit,
            "row_count": rows.len(),
        }),
        rows,
        summary,
    })
}

fn build_documents(ids: &[Uuid]) -> Result<RagContext> {
    let db = get_db()?;
    let mut rows: Vec<ContextRow> = Vec::with_capacity(ids.len());
    let mut summary_parts: Vec<String> = Vec::with_capacity(ids.len());
    for (i, id) in ids.iter().enumerate() {
        let md = match db.doc_get_metadata(*id)? {
            Some(m) => m,
            None    => continue,
        };
        let body = match db.doc_get_content(*id)? {
            Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            None        => String::new(),
        };
        let merged = json!({"id": id.to_string(), "metadata": md, "content": body});
        let fp = json_fingerprint(&merged);
        summary_parts.push(format!("[document {}] {}", i + 1, fp));
        rows.push(ContextRow { raw: merged, fingerprint: fp });
    }
    Ok(RagContext {
        n_rows: rows.len(),
        source_meta: json!({
            "kind":      "documents",
            "ids":       ids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
            "row_count": rows.len(),
        }),
        rows,
        summary: summary_parts.join("\n"),
    })
}

fn build_supplied(rows_json: Vec<JsonValue>) -> RagContext {
    let mut rows: Vec<ContextRow> = Vec::with_capacity(rows_json.len());
    let mut parts: Vec<String> = Vec::with_capacity(rows_json.len());
    for (i, row) in rows_json.iter().enumerate() {
        let fp = json_fingerprint(row);
        parts.push(format!("[row {}] {}", i + 1, fp));
        rows.push(ContextRow { raw: row.clone(), fingerprint: fp });
    }
    RagContext {
        n_rows: rows.len(),
        source_meta: json!({
            "kind":      "supplied",
            "row_count": rows.len(),
        }),
        rows,
        summary: parts.join("\n"),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Row-extraction helpers
// ─────────────────────────────────────────────────────────────────────

/// Pull an array out of an analysis Map by trying each candidate field.
/// Some analysis helpers also return a top-level Array directly — we
/// accept that too.
fn pick_rows_array(json: &JsonValue, candidates: &[&str]) -> Vec<JsonValue> {
    if let Some(arr) = json.as_array() {
        return arr.clone();
    }
    for f in candidates {
        if let Some(arr) = json.get(*f).and_then(|v| v.as_array()) {
            return arr.clone();
        }
    }
    Vec::new()
}

fn fingerprint_rows(rows: &[JsonValue], limit: usize) -> Vec<ContextRow> {
    rows.iter().take(limit).filter_map(|r| {
        let fp = json_fingerprint(r);
        if fp.is_empty() { None } else {
            Some(ContextRow { raw: r.clone(), fingerprint: fp })
        }
    }).collect()
}

fn join_labelled(label: &str, rows: &[ContextRow]) -> String {
    rows.iter().enumerate()
        .map(|(i, r)| format!("[{label} {}] {}", i + 1, r.fingerprint))
        .collect::<Vec<_>>()
        .join("\n")
}
