//! `v3/textrank.templates`, `v3/summary_for_recent`, `v3/summary_for_query`,
//! `v3/summary_lsa_for_recent`, `v3/summary_lsa_for_query` — cluster-wide
//! extractive summaries.
//!
//! Each underlying v2 method returns a single string in the `summary`
//! field.  Summaries are not mergeable (concatenating extractive
//! summaries from different corpora produces noise), so the merge
//! strategy mirrors `v3/topics` and `v3/rca`: fan out the v2 method to
//! every Alive peer plus the local node, then **pick the response with
//! the longest `summary` text**.  Summary length is a reasonable proxy
//! for richest input corpus — a peer that saw more events produces a
//! richer extractive summary.  In a fully-replicated steady state every
//! peer's summary is roughly identical and the pick is arbitrary.
//!
//! Each response also gets a `cluster_meta` block so the bdsweb badge
//! can show the per-call peer status.
//!
//! Param shapes are forwarded verbatim from the v2 methods (`session`,
//! `duration` / `query`, `max_sentences`, `ratio`, `min_word_len`,
//! plus method-specific knobs).

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::{fanout, merge};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use uuid::Uuid;

// ── Common merge helper ──────────────────────────────────────────────────────

/// Pick the longest `summary` across local + peers, then attach
/// `cluster_meta`.  Thin wrapper around `merge::pick_longest_string` that
/// also takes ownership of `fan` so it can be passed to `v3_cluster_meta`.
fn pick_longest_summary(
    local: JsonValue,
    fan:   Option<fanout::FanOutResults>,
) -> JsonValue {
    let mut best = merge::pick_longest_string(&local, fan.as_ref(), "summary");
    if let Some(obj) = best.as_object_mut() {
        obj.insert("cluster_meta".into(), v3_cluster_meta(fan));
    }
    best
}

// ── v3/textrank.templates ────────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone)]
struct TplParams {
    #[serde(default)] session: String,
    duration: String,
    #[serde(default)] max_sentences: usize,
    #[serde(default = "d_ratio")] ratio: f32,
    #[serde(default = "d_mwl")]   min_word_len: usize,
    #[serde(default = "d_damping")] damping: f32,
    #[serde(default = "d_iters")]   iters: usize,
    #[serde(default = "d_tol")]     tolerance: f32,
}
fn d_ratio()   -> f32   { 0.3 }
fn d_mwl()     -> usize { 2 }
fn d_damping() -> f32   { 0.85 }
fn d_iters()   -> usize { 30 }
fn d_tol()     -> f32   { 1e-4 }

fn register_textrank_templates(module: &mut RpcModule<()>) {
    module.register_async_method("v3/textrank.templates", |params, _ctx, _| async move {
        let p: TplParams = params.parse()?;

        let p_local = p.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let dur = humantime::parse_duration(&p_local.duration)
                .map_err(|e| rpc_err(-32600, format!("invalid duration {:?}: {e}", p_local.duration)))?;
            let session_id = Uuid::parse_str(&p_local.session).unwrap_or_else(|_| Uuid::nil());
            let cfg = bdslib::TextRankConfig {
                max_sentences: p_local.max_sentences,
                ratio:         p_local.ratio,
                min_word_len:  p_local.min_word_len,
                damping:       p_local.damping,
                iters:         p_local.iters,
                tolerance:     p_local.tolerance,
            };
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let summary = db.textrank_templates(session_id, dur, &cfg)
                .map_err(|e| rpc_err(-32004, e))?;
            Ok(serde_json::json!({
                "duration":      p_local.duration,
                "max_sentences": p_local.max_sentences,
                "ratio":         p_local.ratio,
                "summary":       summary,
            }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let v2_params = serde_json::json!({
            "session": p.session, "duration": p.duration,
            "max_sentences": p.max_sentences, "ratio": p.ratio,
            "min_word_len": p.min_word_len, "damping": p.damping,
            "iters": p.iters, "tolerance": p.tolerance,
        });
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/textrank.templates", v2_params).await),
            None    => None,
        };

        Ok::<JsonValue, ErrorObject>(pick_longest_summary(local, fan))
    }).unwrap();
}

// ── v3/summary_for_recent ────────────────────────────────────────────────────

fn register_summary_for_recent(module: &mut RpcModule<()>) {
    module.register_async_method("v3/summary_for_recent", |params, _ctx, _| async move {
        let p: TplParams = params.parse()?;

        let p_local = p.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let dur = humantime::parse_duration(&p_local.duration)
                .map_err(|e| rpc_err(-32600, format!("invalid duration {:?}: {e}", p_local.duration)))?;
            let txn_id = Uuid::parse_str(&p_local.session).unwrap_or_else(|_| Uuid::nil());
            let cfg = bdslib::TextRankConfig {
                max_sentences: p_local.max_sentences,
                ratio:         p_local.ratio,
                min_word_len:  p_local.min_word_len,
                damping:       p_local.damping,
                iters:         p_local.iters,
                tolerance:     p_local.tolerance,
            };
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let summary = db.summary_for_recent(txn_id, dur, &cfg)
                .map_err(|e| rpc_err(-32004, e))?;
            Ok(serde_json::json!({
                "duration":      p_local.duration,
                "max_sentences": p_local.max_sentences,
                "ratio":         p_local.ratio,
                "summary":       summary,
            }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let v2_params = serde_json::json!({
            "session": p.session, "duration": p.duration,
            "max_sentences": p.max_sentences, "ratio": p.ratio,
            "min_word_len": p.min_word_len, "damping": p.damping,
            "iters": p.iters, "tolerance": p.tolerance,
        });
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/summary_for_recent", v2_params).await),
            None    => None,
        };

        Ok::<JsonValue, ErrorObject>(pick_longest_summary(local, fan))
    }).unwrap();
}

// ── v3/summary_for_query ─────────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone)]
struct QueryParams {
    #[serde(default)] session: String,
    query: String,
    #[serde(default)] max_sentences: usize,
    #[serde(default = "d_ratio")] ratio: f32,
    #[serde(default = "d_mwl")]   min_word_len: usize,
    #[serde(default = "d_damping")] damping: f32,
    #[serde(default = "d_iters")]   iters: usize,
    #[serde(default = "d_tol")]     tolerance: f32,
}

fn register_summary_for_query(module: &mut RpcModule<()>) {
    module.register_async_method("v3/summary_for_query", |params, _ctx, _| async move {
        let p: QueryParams = params.parse()?;

        let p_local = p.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let txn_id = Uuid::parse_str(&p_local.session).unwrap_or_else(|_| Uuid::nil());
            let cfg = bdslib::TextRankConfig {
                max_sentences: p_local.max_sentences,
                ratio:         p_local.ratio,
                min_word_len:  p_local.min_word_len,
                damping:       p_local.damping,
                iters:         p_local.iters,
                tolerance:     p_local.tolerance,
            };
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let summary = db.summary_for_query(txn_id, &p_local.query, &cfg)
                .map_err(|e| rpc_err(-32004, e))?;
            Ok(serde_json::json!({
                "query":         p_local.query,
                "max_sentences": p_local.max_sentences,
                "ratio":         p_local.ratio,
                "summary":       summary,
            }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let v2_params = serde_json::json!({
            "session": p.session, "query": p.query,
            "max_sentences": p.max_sentences, "ratio": p.ratio,
            "min_word_len": p.min_word_len, "damping": p.damping,
            "iters": p.iters, "tolerance": p.tolerance,
        });
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/summary_for_query", v2_params).await),
            None    => None,
        };

        Ok::<JsonValue, ErrorObject>(pick_longest_summary(local, fan))
    }).unwrap();
}

// ── v3/summary_lsa_for_recent ────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone)]
struct LsaTplParams {
    #[serde(default)] session: String,
    duration: String,
    #[serde(default)] max_sentences: usize,
    #[serde(default = "d_ratio")] ratio: f32,
    #[serde(default = "d_mwl")]   min_word_len: usize,
    #[serde(default = "d_n_concepts")] n_concepts: usize,
    #[serde(default = "d_power_iters")] power_iters: usize,
}
fn d_n_concepts()  -> usize { 3 }
fn d_power_iters() -> usize { 50 }

fn register_summary_lsa_for_recent(module: &mut RpcModule<()>) {
    module.register_async_method("v3/summary_lsa_for_recent", |params, _ctx, _| async move {
        let p: LsaTplParams = params.parse()?;

        let p_local = p.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let dur = humantime::parse_duration(&p_local.duration)
                .map_err(|e| rpc_err(-32600, format!("invalid duration {:?}: {e}", p_local.duration)))?;
            let txn_id = Uuid::parse_str(&p_local.session).unwrap_or_else(|_| Uuid::nil());
            let cfg = bdslib::LsaConfig {
                max_sentences: p_local.max_sentences,
                ratio:         p_local.ratio,
                min_word_len:  p_local.min_word_len,
                n_concepts:    p_local.n_concepts,
                power_iters:   p_local.power_iters,
            };
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let summary = db.summary_lsa_for_recent(txn_id, dur, &cfg)
                .map_err(|e| rpc_err(-32004, e))?;
            Ok(serde_json::json!({
                "duration":      p_local.duration,
                "max_sentences": p_local.max_sentences,
                "ratio":         p_local.ratio,
                "summary":       summary,
            }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let v2_params = serde_json::json!({
            "session": p.session, "duration": p.duration,
            "max_sentences": p.max_sentences, "ratio": p.ratio,
            "min_word_len": p.min_word_len,
            "n_concepts": p.n_concepts, "power_iters": p.power_iters,
        });
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/summary_lsa_for_recent", v2_params).await),
            None    => None,
        };

        Ok::<JsonValue, ErrorObject>(pick_longest_summary(local, fan))
    }).unwrap();
}

// ── v3/summary_lsa_for_query ─────────────────────────────────────────────────

#[derive(serde::Deserialize, Clone)]
struct LsaQueryParams {
    #[serde(default)] session: String,
    query: String,
    #[serde(default)] max_sentences: usize,
    #[serde(default = "d_ratio")] ratio: f32,
    #[serde(default = "d_mwl")]   min_word_len: usize,
    #[serde(default = "d_n_concepts")] n_concepts: usize,
    #[serde(default = "d_power_iters")] power_iters: usize,
}

fn register_summary_lsa_for_query(module: &mut RpcModule<()>) {
    module.register_async_method("v3/summary_lsa_for_query", |params, _ctx, _| async move {
        let p: LsaQueryParams = params.parse()?;

        let p_local = p.clone();
        let local = tokio::task::spawn_blocking(move || -> Result<JsonValue, ErrorObject<'static>> {
            let txn_id = Uuid::parse_str(&p_local.session).unwrap_or_else(|_| Uuid::nil());
            let cfg = bdslib::LsaConfig {
                max_sentences: p_local.max_sentences,
                ratio:         p_local.ratio,
                min_word_len:  p_local.min_word_len,
                n_concepts:    p_local.n_concepts,
                power_iters:   p_local.power_iters,
            };
            let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
            let summary = db.summary_lsa_for_query(txn_id, &p_local.query, &cfg)
                .map_err(|e| rpc_err(-32004, e))?;
            Ok(serde_json::json!({
                "query":         p_local.query,
                "max_sentences": p_local.max_sentences,
                "ratio":         p_local.ratio,
                "summary":       summary,
            }))
        }).await.map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        let v2_params = serde_json::json!({
            "session": p.session, "query": p.query,
            "max_sentences": p.max_sentences, "ratio": p.ratio,
            "min_word_len": p.min_word_len,
            "n_concepts": p.n_concepts, "power_iters": p.power_iters,
        });
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fan = match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/summary_lsa_for_query", v2_params).await),
            None    => None,
        };

        Ok::<JsonValue, ErrorObject>(pick_longest_summary(local, fan))
    }).unwrap();
}

// ── Public entry ─────────────────────────────────────────────────────────────

pub fn register(module: &mut RpcModule<()>) {
    register_textrank_templates(module);
    register_summary_for_recent(module);
    register_summary_for_query(module);
    register_summary_lsa_for_recent(module);
    register_summary_lsa_for_query(module);
}
