//! Analytics surface — n-gram anomaly / denoise, k-NN, RCA, topics,
//! trends, summaries, timeline.
//!
//! All cluster-aware reads.  Cluster merge strategies mirror the
//! corresponding bdsnode `v3_*` handlers exactly:
//!
//! | Helper                         | Cluster strategy                                  |
//! |--------------------------------|---------------------------------------------------|
//! | `anomaly_recent`               | Gather UUID-deduped fingerprints, run analysis on union |
//! | `denoise_recent`               | Same                                              |
//! | `knn`                          | Same                                              |
//! | `rca` / `rca_templates`        | pick response with largest `n_events`             |
//! | `topics` (one key)             | pick response with largest LDA corpus             |
//! | `topics_all`                   | per-key, pick with largest LDA corpus             |
//! | `trends`                       | pick response with largest sample count `n`       |
//! | `textrank_templates` / summaries | pick response with longest `summary` string     |
//! | `timeline`                     | min(min_ts), max(max_ts) across local + peers     |

use crate::analysis::knn::{knn_summary_with, KnnConfig};
use crate::analysis::ngram::{ngram_anomaly_with, ngram_remove_noise_with, NgramAnomalyConfig, NgramNoiseConfig};
use crate::cluster::merge;
use crate::vm::api::dispatch;
use crate::vm::api::runtime;
use crate::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use crate::{LdaConfig, LsaConfig, RcaConfig, RcaResult, RcaTemplatesConfig, RcaTemplatesResult,
            TelemetryTrend, TextRankConfig, TopicSummary};
use easy_error::{err_msg, Error};
use rust_dynamic::value::Value;
use serde_json::{json, Value as JsonValue};
use std::collections::HashSet;
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────────────
// Cluster fingerprint gather (used by anomaly_recent / denoise_recent / knn).
// ─────────────────────────────────────────────────────────────────────────────

/// Mirrors `bdsnode/jsonrpc/v3_helpers::gather_cluster_fingerprints` —
/// fetch local fingerprints, fan out `v2/fingerprints.recent` to peers,
/// dedup by UUID (first-seen wins, local first).
fn gather_fingerprints(duration: &str) -> Result<(Vec<String>, usize), Error> {
    let dur = humantime::parse_duration(duration)
        .map_err(|e| err_msg(format!("vm::api::gather_fingerprints: parse duration {duration:?}: {e}")))?;
    let db = crate::globals::get_db()
        .map_err(|e| err_msg(format!("vm::api::gather_fingerprints: get_db: {e}")))?;
    let local_pairs = db.fingerprints_with_ids_in_recent(dur)
        .map_err(|e| err_msg(format!("vm::api::gather_fingerprints: db: {e}")))?;
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut raw_total = local_pairs.len();
    for (id, fp) in local_pairs { if seen.insert(id) { out.push(fp); } }

    if let Some(cluster) = db.cluster() {
        let cluster = cluster.clone();
        let params = json!({"duration": duration});
        let fan = runtime::block_on(crate::cluster::fanout::fan_out_v2(
            &cluster, "v2/fingerprints.recent", params,
        ));
        crate::vm::api::meta::set({
            let mut m = fan.cluster_meta();
            if let Some(obj) = m.as_object_mut() {
                obj.insert("enabled".into(), JsonValue::Bool(true));
            }
            m
        });
        for body in fan.ok_results() {
            if let Some(arr) = body.get("fingerprints").and_then(|v| v.as_array()) {
                raw_total += arr.len();
                for item in arr {
                    let id_str = match item.get("id").and_then(|v| v.as_str()) {
                        Some(s) => s, None => continue,
                    };
                    let id = match Uuid::parse_str(id_str) { Ok(u) => u, Err(_) => continue };
                    if !seen.insert(id) { continue; }
                    if let Some(fp) = item.get("fingerprint").and_then(|v| v.as_str()) {
                        if !fp.trim().is_empty() { out.push(fp.to_owned()); }
                    }
                }
            }
        }
    } else {
        crate::vm::api::meta::clear();
    }
    Ok((out, raw_total))
}

/// Read an integer field from a Map with a default.
fn opt_u(opts: &JsonValue, key: &str, dflt: u64) -> u64 {
    opts.get(key).and_then(|v| v.as_u64()).unwrap_or(dflt)
}
fn opt_f(opts: &JsonValue, key: &str, dflt: f64) -> f64 {
    opts.get(key).and_then(|v| v.as_f64()).unwrap_or(dflt)
}
fn opt_s(opts: &JsonValue, key: &str) -> Option<String> {
    opts.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

// ─────────────────────────────────────────────────────────────────────────────
// Anomaly / denoise / kNN — coordinator-side analysis on cluster union.
// ─────────────────────────────────────────────────────────────────────────────

/// N-gram anomaly detection over the trailing duration window.  In
/// cluster mode the input corpus is the UUID-deduped union of every
/// peer's fingerprints; the analysis runs once on the coordinator.
pub fn anomaly_recent(duration: &str, opts: Value) -> Result<Value, Error> {
    let opts_json = dynamic_to_json(opts);
    let cfg = NgramAnomalyConfig {
        n:                 opt_u(&opts_json, "n", 2) as usize,
        min_word_len:      opt_u(&opts_json, "min_word_len", 2) as usize,
        anomaly_threshold: opt_f(&opts_json, "anomaly_threshold", 0.7) as f32,
        max_anomalies:     opt_u(&opts_json, "max_anomalies", 20) as usize,
        max_novel_ngrams:  opt_u(&opts_json, "max_novel_ngrams", 5) as usize,
    };
    let (fps, raw_total) = gather_fingerprints(duration)?;
    let n_unique = fps.len();
    let mut out = ngram_anomaly_with(&fps, &cfg);
    if let Some(obj) = out.as_object_mut() {
        obj.insert("n_unique_fingerprints".into(), JsonValue::from(n_unique));
        obj.insert("n_raw_fingerprints".into(),    JsonValue::from(raw_total));
    }
    Ok(json_to_dynamic(out))
}

/// N-gram noise removal over the trailing duration window.  Same
/// gather strategy as `anomaly_recent`.
pub fn denoise_recent(duration: &str, opts: Value) -> Result<Value, Error> {
    let opts_json = dynamic_to_json(opts);
    let cfg = NgramNoiseConfig {
        n:               opt_u(&opts_json, "n", 2) as usize,
        min_word_len:    opt_u(&opts_json, "min_word_len", 2) as usize,
        noise_threshold: opt_f(&opts_json, "noise_threshold", 0.85) as f32,
        max_kept:        opt_u(&opts_json, "max_kept", 100) as usize,
        max_removed:     opt_u(&opts_json, "max_removed", 100) as usize,
    };
    let (fps, raw_total) = gather_fingerprints(duration)?;
    let n_unique = fps.len();
    let mut out = ngram_remove_noise_with(&fps, &cfg);
    if let Some(obj) = out.as_object_mut() {
        obj.insert("n_unique_fingerprints".into(), JsonValue::from(n_unique));
        obj.insert("n_raw_fingerprints".into(),    JsonValue::from(raw_total));
    }
    Ok(json_to_dynamic(out))
}

/// k-NN intelligence over the trailing duration window.  Same gather
/// strategy as `anomaly_recent`.
pub fn knn(duration: &str, opts: Value) -> Result<Value, Error> {
    let opts_json = dynamic_to_json(opts);
    let cfg = KnnConfig {
        k:                   opt_u(&opts_json, "k", 5) as usize,
        min_word_len:        opt_u(&opts_json, "min_word_len", 2) as usize,
        anomaly_threshold:   opt_f(&opts_json, "anomaly_threshold", 0.2) as f32,
        max_cluster_members: opt_u(&opts_json, "max_cluster_members", 10) as usize,
        max_anomalies:       opt_u(&opts_json, "max_anomalies", 20) as usize,
    };
    let (fps, raw_total) = gather_fingerprints(duration)?;
    let n_unique = fps.len();
    let mut out = knn_summary_with(&fps, &cfg);
    if let Some(obj) = out.as_object_mut() {
        obj.insert("n_unique_fingerprints".into(), JsonValue::from(n_unique));
        obj.insert("n_raw_fingerprints".into(),    JsonValue::from(raw_total));
    }
    Ok(json_to_dynamic(out))
}

// ─────────────────────────────────────────────────────────────────────────────
// RCA — pick by largest n_events.
// ─────────────────────────────────────────────────────────────────────────────

fn rca_inner(method: &'static str, opts: Value, templates: bool) -> Result<Value, Error> {
    let raw = dynamic_to_json(opts);
    let merged = dispatch::read(
        method,
        raw.clone(),
        move || {
            let dur = opt_s(&raw, "duration")
                .ok_or_else(|| err_msg("vm::api::rca: missing 'duration'"))?;
            humantime::parse_duration(&dur)
                .map_err(|e| err_msg(format!("vm::api::rca: parse duration {dur:?}: {e}")))?;
            let failure_key = opt_s(&raw, "failure_key");
            let bucket_secs = opt_u(&raw, "bucket_secs", 300);
            let min_support = opt_u(&raw, "min_support", 2) as usize;
            let jaccard     = opt_f(&raw, "jaccard_threshold", 0.2);
            let max_keys    = opt_u(&raw, "max_keys", 200) as usize;
            if templates {
                let cfg = RcaTemplatesConfig { bucket_secs, min_support, jaccard_threshold: jaccard, max_keys };
                let db = crate::globals::get_db()
                    .map_err(|e| err_msg(format!("vm::api::rca: get_db: {e}")))?;
                let r = match &failure_key {
                    Some(fk) => RcaTemplatesResult::analyze_failure(db, fk, &dur, &cfg),
                    None     => RcaTemplatesResult::analyze(db, &dur, &cfg),
                }.map_err(|e| err_msg(format!("vm::api::rca_templates: db: {e}")))?;
                serde_json::to_value(&r).map_err(|e| err_msg(format!("serialise: {e}")))
            } else {
                let cfg = RcaConfig { bucket_secs, min_support, jaccard_threshold: jaccard, max_keys };
                let r = match &failure_key {
                    Some(fk) => RcaResult::analyze_failure(fk, &dur, &cfg),
                    None     => RcaResult::analyze(&dur, &cfg),
                }.map_err(|e| err_msg(format!("vm::api::rca: db: {e}")))?;
                serde_json::to_value(&r).map_err(|e| err_msg(format!("serialise: {e}")))
            }
        },
        |local, fan| {
            let (mut chosen, n) = merge::pick_largest_by_field(&local, fan, "n_events");
            if let Some(obj) = chosen.as_object_mut() {
                obj.insert("corpus_size".into(), JsonValue::from(n));
            }
            chosen
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// Root-cause analysis over event clusters.  `opts` Map fields:
/// `duration` (req), `failure_key`, `bucket_secs`, `min_support`,
/// `jaccard_threshold`, `max_keys`.
pub fn rca(opts: Value) -> Result<Value, Error> { rca_inner("v2/rca", opts, false) }

/// RCA over template clusters (drain3 templates instead of raw events).
pub fn rca_templates(opts: Value) -> Result<Value, Error> { rca_inner("v2/rca.templates", opts, true) }

// ─────────────────────────────────────────────────────────────────────────────
// Topics (LDA) — pick by largest corpus.
// ─────────────────────────────────────────────────────────────────────────────

/// LDA topic summary for a single key.  `opts` Map fields:
/// `key` (req), `duration` (req), `k`, `alpha`, `beta`, `seed`,
/// `iters`, `top_n`.
pub fn topics(opts: Value) -> Result<Value, Error> {
    let raw = dynamic_to_json(opts);
    let merged = dispatch::read(
        "v2/topics",
        raw.clone(),
        move || {
            let key = opt_s(&raw, "key").ok_or_else(|| err_msg("vm::api::topics: missing 'key'"))?;
            let dur = opt_s(&raw, "duration").ok_or_else(|| err_msg("vm::api::topics: missing 'duration'"))?;
            humantime::parse_duration(&dur)
                .map_err(|e| err_msg(format!("vm::api::topics: parse duration: {e}")))?;
            let cfg = LdaConfig {
                k:     opt_u(&raw, "k", 3) as usize,
                alpha: opt_f(&raw, "alpha", 0.1),
                beta:  opt_f(&raw, "beta", 0.01),
                seed:  opt_u(&raw, "seed", 42),
                iters: opt_u(&raw, "iters", 200) as usize,
                top_n: opt_u(&raw, "top_n", 10) as usize,
            };
            let summary = TopicSummary::query_window(&key, &dur, cfg)
                .map_err(|e| err_msg(format!("vm::api::topics: db: {e}")))?;
            serde_json::to_value(&summary).map_err(|e| err_msg(format!("serialise: {e}")))
        },
        |local, fan| {
            let local_n = merge::lda_corpus_size(&local);
            let (mut best, mut best_n) = (local, local_n);
            if let Some(f) = fan {
                for r in f.ok_results() {
                    let n = merge::lda_corpus_size(r);
                    if n > best_n { best = r.clone(); best_n = n; }
                }
            }
            if let Some(obj) = best.as_object_mut() {
                obj.insert("corpus_size".into(), JsonValue::from(best_n));
            }
            best
        },
    )?;
    Ok(json_to_dynamic(merged))
}

/// LDA topic summary for **every** key.  `opts` same shape as `topics`
/// but no `key` field.  Cluster merge: per-key pick by largest corpus.
pub fn topics_all(opts: Value) -> Result<Value, Error> {
    let raw = dynamic_to_json(opts);
    let merged = dispatch::read(
        "v2/topics.all",
        raw.clone(),
        move || {
            let dur = opt_s(&raw, "duration").ok_or_else(|| err_msg("vm::api::topics_all: missing 'duration'"))?;
            humantime::parse_duration(&dur)
                .map_err(|e| err_msg(format!("vm::api::topics_all: parse duration: {e}")))?;
            let cfg = LdaConfig {
                k:     opt_u(&raw, "k", 3) as usize,
                alpha: opt_f(&raw, "alpha", 0.1),
                beta:  opt_f(&raw, "beta", 0.01),
                seed:  opt_u(&raw, "seed", 42),
                iters: opt_u(&raw, "iters", 200) as usize,
                top_n: opt_u(&raw, "top_n", 10) as usize,
            };
            let summaries = TopicSummary::query_all_keys(&dur, cfg)
                .map_err(|e| err_msg(format!("vm::api::topics_all: db: {e}")))?;
            let topics: Vec<JsonValue> = summaries.into_iter()
                .map(|s| serde_json::to_value(&s).unwrap_or(JsonValue::Null))
                .collect();
            Ok(json!({"topics": topics}))
        },
        |local, fan| {
            let topics = merge::pick_largest_per_key(&local, fan, "topics", "key", merge::lda_corpus_size);
            json!({"topics": topics})
        },
    )?;
    Ok(json_to_dynamic(merged))
}

// ─────────────────────────────────────────────────────────────────────────────
// Trends — pick by largest sample count.
// ─────────────────────────────────────────────────────────────────────────────

/// Per-key statistical trend (mean / median / std-dev / anomalies /
/// breakouts).  Cluster pick: largest sample count `n`.
pub fn trends(key: &str, duration: &str) -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/trends",
        json!({"session": "", "key": key, "duration": duration}),
        move || {
            humantime::parse_duration(duration)
                .map_err(|e| err_msg(format!("vm::api::trends: parse duration: {e}")))?;
            let trend = TelemetryTrend::query_window(key, duration)
                .map_err(|e| err_msg(format!("vm::api::trends: db: {e}")))?;
            serde_json::to_value(&trend).map_err(|e| err_msg(format!("serialise: {e}")))
        },
        |local, fan| merge::pick_largest_by_field(&local, fan, "n").0,
    )?;
    Ok(json_to_dynamic(merged))
}

// ─────────────────────────────────────────────────────────────────────────────
// Summaries — TextRank + LSA.  Cluster pick: longest summary string.
// ─────────────────────────────────────────────────────────────────────────────

/// Build a TextRank summary of drain3 templates over `duration`.
pub fn textrank_templates(duration: &str, opts: Value) -> Result<Value, Error> {
    let raw = dynamic_to_json(opts);
    let merged = dispatch::read(
        "v2/textrank.templates",
        merge_into(json!({"session": "", "duration": duration}), &raw),
        move || {
            let cfg = textrank_cfg(&raw);
            let dur = humantime::parse_duration(duration)
                .map_err(|e| err_msg(format!("vm::api::textrank_templates: parse duration: {e}")))?;
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::textrank_templates: get_db: {e}")))?;
            let summary = db.textrank_templates(Uuid::nil(), dur, &cfg)
                .map_err(|e| err_msg(format!("vm::api::textrank_templates: db: {e}")))?;
            Ok(json!({"duration": duration, "max_sentences": cfg.max_sentences, "ratio": cfg.ratio, "summary": summary}))
        },
        |local, fan| merge::pick_longest_string(&local, fan, "summary"),
    )?;
    Ok(json_to_dynamic(merged))
}

/// TextRank summary over recent observability bodies.
pub fn summary_for_recent(duration: &str, opts: Value) -> Result<Value, Error> {
    let raw = dynamic_to_json(opts);
    let merged = dispatch::read(
        "v2/summary_for_recent",
        merge_into(json!({"session": "", "duration": duration}), &raw),
        move || {
            let cfg = textrank_cfg(&raw);
            let dur = humantime::parse_duration(duration)
                .map_err(|e| err_msg(format!("vm::api::summary_for_recent: parse duration: {e}")))?;
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::summary_for_recent: get_db: {e}")))?;
            let summary = db.summary_for_recent(Uuid::nil(), dur, &cfg)
                .map_err(|e| err_msg(format!("vm::api::summary_for_recent: db: {e}")))?;
            Ok(json!({"duration": duration, "max_sentences": cfg.max_sentences, "ratio": cfg.ratio, "summary": summary}))
        },
        |local, fan| merge::pick_longest_string(&local, fan, "summary"),
    )?;
    Ok(json_to_dynamic(merged))
}

/// TextRank summary over vector-search hits matching `query`.
pub fn summary_for_query(query: &str, opts: Value) -> Result<Value, Error> {
    let raw = dynamic_to_json(opts);
    let merged = dispatch::read(
        "v2/summary_for_query",
        merge_into(json!({"session": "", "query": query}), &raw),
        move || {
            let cfg = textrank_cfg(&raw);
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::summary_for_query: get_db: {e}")))?;
            let summary = db.summary_for_query(Uuid::nil(), query, &cfg)
                .map_err(|e| err_msg(format!("vm::api::summary_for_query: db: {e}")))?;
            Ok(json!({"query": query, "max_sentences": cfg.max_sentences, "ratio": cfg.ratio, "summary": summary}))
        },
        |local, fan| merge::pick_longest_string(&local, fan, "summary"),
    )?;
    Ok(json_to_dynamic(merged))
}

/// LSA summary over recent observability bodies.
pub fn summary_lsa_for_recent(duration: &str, opts: Value) -> Result<Value, Error> {
    let raw = dynamic_to_json(opts);
    let merged = dispatch::read(
        "v2/summary_lsa_for_recent",
        merge_into(json!({"session": "", "duration": duration}), &raw),
        move || {
            let cfg = lsa_cfg(&raw);
            let dur = humantime::parse_duration(duration)
                .map_err(|e| err_msg(format!("vm::api::summary_lsa_for_recent: parse duration: {e}")))?;
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::summary_lsa_for_recent: get_db: {e}")))?;
            let summary = db.summary_lsa_for_recent(Uuid::nil(), dur, &cfg)
                .map_err(|e| err_msg(format!("vm::api::summary_lsa_for_recent: db: {e}")))?;
            Ok(json!({"duration": duration, "max_sentences": cfg.max_sentences, "ratio": cfg.ratio, "summary": summary}))
        },
        |local, fan| merge::pick_longest_string(&local, fan, "summary"),
    )?;
    Ok(json_to_dynamic(merged))
}

/// LSA summary over vector-search hits matching `query`.
pub fn summary_lsa_for_query(query: &str, opts: Value) -> Result<Value, Error> {
    let raw = dynamic_to_json(opts);
    let merged = dispatch::read(
        "v2/summary_lsa_for_query",
        merge_into(json!({"session": "", "query": query}), &raw),
        move || {
            let cfg = lsa_cfg(&raw);
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::summary_lsa_for_query: get_db: {e}")))?;
            let summary = db.summary_lsa_for_query(Uuid::nil(), query, &cfg)
                .map_err(|e| err_msg(format!("vm::api::summary_lsa_for_query: db: {e}")))?;
            Ok(json!({"query": query, "max_sentences": cfg.max_sentences, "ratio": cfg.ratio, "summary": summary}))
        },
        |local, fan| merge::pick_longest_string(&local, fan, "summary"),
    )?;
    Ok(json_to_dynamic(merged))
}

fn textrank_cfg(opts: &JsonValue) -> TextRankConfig {
    TextRankConfig {
        max_sentences: opt_u(opts, "max_sentences", 0) as usize,
        ratio:         opt_f(opts, "ratio", 0.3) as f32,
        min_word_len:  opt_u(opts, "min_word_len", 2) as usize,
        damping:       opt_f(opts, "damping", 0.85) as f32,
        iters:         opt_u(opts, "iters", 30) as usize,
        tolerance:     opt_f(opts, "tolerance", 1e-4) as f32,
    }
}
fn lsa_cfg(opts: &JsonValue) -> LsaConfig {
    LsaConfig {
        max_sentences: opt_u(opts, "max_sentences", 0) as usize,
        ratio:         opt_f(opts, "ratio", 0.3) as f32,
        min_word_len:  opt_u(opts, "min_word_len", 2) as usize,
        n_concepts:    opt_u(opts, "n_concepts", 3) as usize,
        power_iters:   opt_u(opts, "power_iters", 50) as usize,
    }
}

/// Merge `extra` (Map) on top of `base` (Map) — `base` wins for
/// already-set keys, `extra` fills gaps.  Used to fold caller `opts`
/// into the base v2 fan-out params without overriding required fields.
fn merge_into(mut base: JsonValue, extra: &JsonValue) -> JsonValue {
    if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    base
}

// ─────────────────────────────────────────────────────────────────────────────
// Timeline — min/max ts across cluster.
// ─────────────────────────────────────────────────────────────────────────────

/// Earliest and latest record timestamps across the cluster.
pub fn timeline() -> Result<Value, Error> {
    let merged = dispatch::read(
        "v2/timeline",
        json!({}),
        || {
            let db = crate::globals::get_db()
                .map_err(|e| err_msg(format!("vm::api::timeline: get_db: {e}")))?;
            let cache  = db.cache();
            let shards = cache.info().list_all()
                .map_err(|e| err_msg(format!("vm::api::timeline: list shards: {e}")))?;
            let mut min_ts: Option<i64> = None;
            let mut max_ts: Option<i64> = None;
            for info in &shards {
                let s = cache.shard(info.start_time)
                    .map_err(|e| err_msg(format!("vm::api::timeline: shard: {e}")))?;
                let (smin, _) = s.observability().timestamp_range()
                    .map_err(|e| err_msg(format!("vm::api::timeline: ts_range: {e}")))?;
                if smin.is_some() { min_ts = smin; break; }
            }
            for info in shards.iter().rev() {
                let s = cache.shard(info.start_time)
                    .map_err(|e| err_msg(format!("vm::api::timeline: shard: {e}")))?;
                let (_, smax) = s.observability().timestamp_range()
                    .map_err(|e| err_msg(format!("vm::api::timeline: ts_range: {e}")))?;
                if smax.is_some() { max_ts = smax; break; }
            }
            Ok(json!({"min_ts": min_ts, "max_ts": max_ts}))
        },
        |local, fan| {
            let (min_ts, max_ts) = merge::min_max_fields(&local, fan, "min_ts", "max_ts");
            json!({"min_ts": min_ts, "max_ts": max_ts})
        },
    )?;
    Ok(json_to_dynamic(merged))
}
