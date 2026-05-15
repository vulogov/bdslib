//! Analysis → Project Logs.
//!
//! Drives `v?/project_logs` from the bdsweb form: pick `duration_back`
//! (training window) + `duration_forward` (projection window) + a few
//! tuning knobs, render the projected events as a time-binned table,
//! and offer the standard "Analyze this!" floating-pane LLM review.
//!
//! Same v2-or-v3 versioning convention as the other Analysis pages —
//! [`rpc_versioned`] picks v3 in cluster mode (union projection across
//! every Alive peer) and v2 in standalone (local node only).

use askama::Template;
use axum::{
    extract::{Form, Query, State},
    response::Html,
};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

use crate::{
    admin::signed_rpc_with_timeout,
    client::{mode_badge_for_page, rpc_versioned, ModeBadge},
    error::AppError,
    state::AppState,
};

// ── Form / query params ──────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration_back")]
    pub duration_back: String,
    #[serde(default = "default_duration_forward")]
    pub duration_forward: String,
    #[serde(default = "default_min_consensus")]
    pub min_consensus: f64,
    #[serde(default = "default_n_samples")]
    pub n_samples: usize,
    #[serde(default = "default_order")]
    pub order: u8,
    #[serde(default = "default_time_bins")]
    pub time_bins: usize,
}
fn default_duration_back()    -> String { "1h".to_owned() }
fn default_duration_forward() -> String { "30min".to_owned() }
fn default_min_consensus()    -> f64   { 0.10 }
fn default_n_samples()        -> usize { 50 }
fn default_order()            -> u8    { 2 }
fn default_time_bins()        -> usize { 20 }

// ── Page shell ───────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "project_logs.html")]
struct ProjectLogsPage {
    duration_back:    String,
    duration_forward: String,
    min_consensus:    f64,
    n_samples:        usize,
    order:            u8,
    time_bins:        usize,
    mode_badge:       ModeBadge,
    analyze_provider: String,
    analyze_model:    String,
}

pub async fn page(
    State(state): State<AppState>,
    Query(p):     Query<Params>,
) -> Result<Html<String>, AppError> {
    let (mode_badge, (analyze_provider, analyze_model)) = tokio::join!(
        mode_badge_for_page(&state, true),
        crate::client::analyze_provider(&state),
    );
    Ok(Html(ProjectLogsPage {
        duration_back:    if p.duration_back.is_empty()    { default_duration_back() }    else { p.duration_back },
        duration_forward: if p.duration_forward.is_empty() { default_duration_forward() } else { p.duration_forward },
        min_consensus:    p.min_consensus,
        n_samples:        p.n_samples.max(1),
        order:            p.order.max(1).min(4),
        time_bins:        p.time_bins.max(1),
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── Live results fragment ────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ProjectedRow {
    pub offset_secs:     f64,
    pub text:            String,
    pub source_state:    String,
    pub transition_prob: f64,
    /// Color hint for the consensus chip: emerald (>=0.66), amber
    /// (>=0.33), slate (lower).  Tailwind class chosen on the template
    /// side from this string.
    pub consensus_class: &'static str,
}

#[derive(Template)]
#[template(path = "partials/project_logs_results.html")]
struct ProjectLogsResults {
    duration_back:    String,
    duration_forward: String,
    n_projected:      u64,
    n_unique_inputs:  u64,
    n_raw_inputs:     u64,
    events:           Vec<ProjectedRow>,
    mode_badge:       ModeBadge,
    /// `true` when the projection ran but produced zero events.
    /// Drives the "no projection produced" placeholder on the partial.
    is_empty:         bool,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p):     Query<Params>,
) -> Result<Html<String>, AppError> {
    let resp = rpc_versioned(&state, "v2/project_logs", "v3/project_logs", json!({
        "duration_back":    p.duration_back,
        "duration_forward": p.duration_forward,
        "min_consensus":    p.min_consensus,
        "n_samples":        p.n_samples,
        "order":            p.order,
        "time_bins":        p.time_bins,
    })).await?;

    let n_projected     = resp.get("n").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_unique_inputs = resp.get("n_unique_inputs").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_raw_inputs    = resp.get("n_raw_inputs").and_then(JsonValue::as_u64).unwrap_or(0);

    let events: Vec<ProjectedRow> = resp.get("events")
        .and_then(JsonValue::as_array)
        .map(|arr| arr.iter().map(|e| {
            let prob = e.get("transition_prob").and_then(JsonValue::as_f64).unwrap_or(0.0);
            ProjectedRow {
                offset_secs:     e.get("offset_secs").and_then(JsonValue::as_f64).unwrap_or(0.0),
                text:            e.get("text").and_then(|v| v.as_str()).unwrap_or("").to_owned(),
                source_state:    e.get("source_state").and_then(|v| v.as_str()).unwrap_or("").to_owned(),
                transition_prob: prob,
                consensus_class: if prob >= 0.66 { "text-emerald-300" }
                                 else if prob >= 0.33 { "text-amber-300" }
                                 else { "text-slate-400" },
            }
        }).collect())
        .unwrap_or_default();

    let is_empty   = events.is_empty();
    let mode_badge = ModeBadge::from_response(&resp);

    Ok(Html(ProjectLogsResults {
        duration_back:    p.duration_back,
        duration_forward: p.duration_forward,
        n_projected,
        n_unique_inputs,
        n_raw_inputs,
        is_empty,
        events,
        mode_badge,
    }.render()?))
}

// ── HTMX "Analyze this!" — LLM review of the projection ──────────────────────

#[derive(Template)]
#[template(path = "partials/project_logs_analysis.html")]
struct ProjectLogsAnalysis {
    response:         String,
    response_html:    String,
    provider:         String,
    model:            String,
    ms:               u64,
    n_events_fed:     usize,
    n_unique_inputs:  u64,
    duration_back:    String,
    duration_forward: String,
    cache:            String,
    error:            String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Form(p):      Form<Params>,
) -> Result<Html<String>, AppError> {
    let cfg = state.project_logs_analyze.clone();

    // Re-run the projection so the analysis matches what the operator
    // sees on the page.  Failures render in the panel, not 500.
    let resp = match rpc_versioned(&state, "v2/project_logs", "v3/project_logs", json!({
        "duration_back":    p.duration_back,
        "duration_forward": p.duration_forward,
        "min_consensus":    p.min_consensus,
        "n_samples":        p.n_samples,
        "order":            p.order,
        "time_bins":        p.time_bins,
    })).await {
        Ok(v)  => v,
        Err(e) => return Ok(Html(ProjectLogsAnalysis {
            response: String::new(), response_html: String::new(),
            provider: String::new(), model: String::new(),
            ms: 0, n_events_fed: 0, n_unique_inputs: 0,
            duration_back:    p.duration_back.clone(),
            duration_forward: p.duration_forward.clone(),
            cache: String::new(),
            error: format!("Could not fetch projection for analysis: {e}"),
        }.render()?)),
    };

    let n_unique_inputs = resp.get("n_unique_inputs").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_raw_inputs    = resp.get("n_raw_inputs").and_then(JsonValue::as_u64).unwrap_or(0);
    let events_raw: Vec<JsonValue> = resp.get("events")
        .and_then(JsonValue::as_array).cloned().unwrap_or_default();
    let n_projected = events_raw.len();

    if n_projected == 0 {
        return Ok(Html(ProjectLogsAnalysis {
            response: String::new(), response_html: String::new(),
            provider: String::new(), model: String::new(),
            ms: 0, n_events_fed: 0, n_unique_inputs,
            duration_back:    p.duration_back.clone(),
            duration_forward: p.duration_forward.clone(),
            cache: String::new(),
            error: format!(
                "Empty projection from `{}` of training input over `{}` ahead \
                 ({} unique input records).  Try widening `duration_back`, \
                 lowering `min_consensus`, or running on a busier node.",
                p.duration_back, p.duration_forward, n_unique_inputs,
            ),
        }.render()?));
    }

    // Build the supplied payload — same shape as anomaly_recent: one
    // synthetic stats row first so the model anchors on context, then
    // up to `cfg.max_rows - 1` projected-event rows.  Drain3 templates
    // are operator-readable so the model can quote them verbatim.
    let cap = cfg.max_rows.saturating_sub(1);
    let n_events_fed = n_projected.min(cap);
    let mut rows: Vec<JsonValue> = Vec::with_capacity(n_events_fed + 1);
    rows.push(json!({
        "_kind":            "projection_meta",
        "duration_back":    p.duration_back,
        "duration_forward": p.duration_forward,
        "n_unique_inputs":  n_unique_inputs,
        "n_raw_inputs":     n_raw_inputs,
        "n_projected":      n_projected,
        "n_projected_fed":  n_events_fed,
        "min_consensus":    p.min_consensus,
        "n_samples":        p.n_samples,
        "order":            p.order,
        "time_bins":        p.time_bins,
    }));
    for e in events_raw.iter().take(n_events_fed) {
        let mut obj = e.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("_kind".into(), json!("projected_event"));
        }
        rows.push(obj);
    }

    let analyze_resp = signed_rpc_with_timeout(
        &state,
        "v4/llm.analyze",
        json!({
            "kind":            "supplied",
            "rows":            rows,
            "prompt_template": cfg.prompt_template,
        }),
        Some(std::time::Duration::from_secs(cfg.timeout_secs)),
    ).await;

    match analyze_resp {
        Ok(v) => {
            let response = v.get("response").and_then(|x| x.as_str()).unwrap_or("").to_owned();
            let provider = v.get("provider").and_then(|x| x.as_str()).unwrap_or("?").to_owned();
            let model    = v.get("model").and_then(|x| x.as_str()).unwrap_or("?").to_owned();
            let ms       = v.get("ms").and_then(|x| x.as_u64()).unwrap_or(0);
            let cache    = v.get("cache").and_then(|x| x.as_str()).unwrap_or("").to_owned();
            Ok(Html(ProjectLogsAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                n_events_fed, n_unique_inputs,
                duration_back: p.duration_back,
                duration_forward: p.duration_forward,
                cache,
                error: String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(ProjectLogsAnalysis {
            response: String::new(), response_html: String::new(),
            provider: String::new(), model: String::new(),
            ms: 0, n_events_fed, n_unique_inputs,
            duration_back: p.duration_back,
            duration_forward: p.duration_forward,
            cache: String::new(),
            error: format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
