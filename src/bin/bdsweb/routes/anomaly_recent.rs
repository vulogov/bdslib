use askama::Template;
use axum::{extract::{Form, Query, State}, response::Html};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

use crate::{admin::signed_rpc_with_timeout, client::{mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default = "default_n")]
    pub n: usize,
    #[serde(default = "default_min_word_len")]
    pub min_word_len: usize,
    #[serde(default = "default_anomaly_threshold")]
    pub anomaly_threshold: f32,
    #[serde(default = "default_max_anomalies")]
    pub max_anomalies: usize,
}
fn default_duration()          -> String { "1h".to_owned() }
fn default_n()                 -> usize { 2 }
fn default_min_word_len()      -> usize { 2 }
fn default_anomaly_threshold() -> f32   { 0.7 }
fn default_max_anomalies()     -> usize { 20 }

// ── Page shell ────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "anomaly_recent.html")]
struct AnomalyPage {
    duration:          String,
    n:                 usize,
    min_word_len:      usize,
    anomaly_threshold: f32,
    max_anomalies:     usize,
    mode_badge:        ModeBadge,
    /// Default LLM provider id, surfaced on `data-` for the
    /// wait-message JS.  Empty when bdsnode reports no providers.
    analyze_provider:  String,
    /// Default model name.  Empty when unavailable.
    analyze_model:     String,
}

pub async fn page(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let (mode_badge, (analyze_provider, analyze_model)) = tokio::join!(
        mode_badge_for_page(&state, true),
        crate::client::analyze_provider(&state),
    );
    Ok(Html(AnomalyPage {
        duration:          p.duration,
        n:                 p.n,
        min_word_len:      p.min_word_len,
        anomaly_threshold: p.anomaly_threshold,
        max_anomalies:     p.max_anomalies,
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Debug)]
pub struct AnomalyRow {
    pub idx:          u64,
    pub rarity:       f64,
    pub text:         String,
    pub novel_ngrams: Vec<String>,
}

#[derive(Template)]
#[template(path = "partials/anomaly_recent_result.html")]
struct AnomalyResult {
    duration:        String,
    n_logs:          u64,
    n:               u64,
    n_unique_ngrams: u64,
    anomaly_threshold: f64,
    n_anomalies:     u64,
    mean_rarity:     f64,
    has_anomalies:   bool,
    anomalies:       Vec<AnomalyRow>,
    mode_badge:      ModeBadge,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let resp = rpc_versioned(&state, "v2/anomaly.recent", "v3/anomaly.recent", json!({
        "session":           SESSION,
        "duration":          p.duration.clone(),
        "n":                 p.n,
        "min_word_len":      p.min_word_len,
        "anomaly_threshold": p.anomaly_threshold,
        "max_anomalies":     p.max_anomalies,
    })).await?;

    let n_logs           = resp.get("n_logs").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_eff            = resp.get("n").and_then(JsonValue::as_u64).unwrap_or(p.n as u64);
    let n_unique_ngrams  = resp.get("n_unique_ngrams").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_anomalies      = resp.get("n_anomalies").and_then(JsonValue::as_u64).unwrap_or(0);
    let mean_rarity      = resp.get("mean_rarity").and_then(JsonValue::as_f64).unwrap_or(0.0);
    let anomaly_threshold = resp.get("anomaly_threshold").and_then(JsonValue::as_f64)
        .unwrap_or(p.anomaly_threshold as f64);

    let anomalies: Vec<AnomalyRow> = resp.get("anomalies")
        .and_then(JsonValue::as_array)
        .map(|arr| arr.iter().map(|a| {
            let novel: Vec<String> = a.get("novel_ngrams")
                .and_then(JsonValue::as_array)
                .map(|ngs| ngs.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect())
                .unwrap_or_default();
            AnomalyRow {
                idx:          a.get("idx").and_then(JsonValue::as_u64).unwrap_or(0),
                rarity:       a.get("rarity").and_then(JsonValue::as_f64).unwrap_or(0.0),
                text:         a.get("text").and_then(|v| v.as_str()).unwrap_or("").to_owned(),
                novel_ngrams: novel,
            }
        }).collect())
        .unwrap_or_default();

    let has_anomalies = !anomalies.is_empty();
    let mode_badge = ModeBadge::from_response(&resp);

    Ok(Html(AnomalyResult {
        duration:          p.duration,
        n_logs,
        n:                 n_eff,
        n_unique_ngrams,
        anomaly_threshold,
        n_anomalies,
        mean_rarity,
        has_anomalies,
        anomalies,
        mode_badge,
    }.render()?))
}

// ── HTMX: "Analyze this!" — explain the nature of the anomalies ──────────────
//
// Re-runs `v?/anomaly.recent` with the same params so the LLM sees
// exactly what the operator does, then ships one stats row +
// per-anomaly rows to `v4/llm.analyze`.  Stats row carries the
// population context (n_logs, n_unique_ngrams, threshold, mean
// rarity) and is always included; anomaly rows are capped by
// `cfg.max_rows` so a runaway lookback can't blow the prompt
// budget.

#[derive(Template)]
#[template(path = "partials/anomaly_recent_analysis.html")]
struct AnomalyAnalysis {
    response:          String,
    response_html:     String,
    provider:          String,
    model:             String,
    ms:                u64,
    /// Per-corpus counts ACTUALLY fed to the LLM after the
    /// `max_rows` budget split.  May be less than the matched
    /// count when the anomaly set is larger than the budget.
    n_anomalies_fed:   usize,
    /// What `v?/anomaly.recent` reported before the budget cut.
    matched_anomalies: usize,
    n_logs:            u64,
    mean_rarity:       f64,
    anomaly_threshold: f64,
    duration:          String,
    /// `"miss"`, `"hit"`, or `""`.
    cache:             String,
    /// Empty when the LLM ran cleanly; banner message otherwise.
    error:             String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Form(p): Form<Params>,
) -> Result<Html<String>, AppError> {
    let cfg = state.anomaly_recent_analyze.clone();

    // Re-run the same RPC the inline `results` handler used so the
    // analysis matches what the operator is looking at.  Failures
    // render in the panel rather than 500-ing.
    let resp = match rpc_versioned(&state, "v2/anomaly.recent", "v3/anomaly.recent", json!({
        "session":           SESSION,
        "duration":          p.duration.clone(),
        "n":                 p.n,
        "min_word_len":      p.min_word_len,
        "anomaly_threshold": p.anomaly_threshold,
        "max_anomalies":     p.max_anomalies,
    })).await {
        Ok(v)  => v,
        Err(e) => return Ok(Html(AnomalyAnalysis {
            response:          String::new(),
            response_html:     String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:                0,
            n_anomalies_fed:   0,
            matched_anomalies: 0,
            n_logs:            0,
            mean_rarity:       0.0,
            anomaly_threshold: p.anomaly_threshold as f64,
            duration:          p.duration.clone(),
            cache:    String::new(),
            error:    format!("Could not fetch anomaly results for analysis: {e}"),
        }.render()?)),
    };

    let n_logs           = resp.get("n_logs").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_unique_ngrams  = resp.get("n_unique_ngrams").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_eff            = resp.get("n").and_then(JsonValue::as_u64).unwrap_or(p.n as u64);
    let mean_rarity      = resp.get("mean_rarity").and_then(JsonValue::as_f64).unwrap_or(0.0);
    let threshold_used   = resp.get("anomaly_threshold").and_then(JsonValue::as_f64)
        .unwrap_or(p.anomaly_threshold as f64);

    let anomalies_raw: Vec<JsonValue> = resp.get("anomalies")
        .and_then(JsonValue::as_array).cloned().unwrap_or_default();
    let matched_anomalies = anomalies_raw.len();

    if matched_anomalies == 0 {
        return Ok(Html(AnomalyAnalysis {
            response:          String::new(),
            response_html:     String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:                0,
            n_anomalies_fed:   0,
            matched_anomalies: 0,
            n_logs,
            mean_rarity,
            anomaly_threshold: threshold_used,
            duration:          p.duration.clone(),
            cache:    String::new(),
            error:    format!(
                "No anomalies above threshold {:.2} in the last {} (scanned {} record{}, \
                 mean rarity {:.3}) — try lowering the threshold or widening the duration.",
                threshold_used, p.duration, n_logs, if n_logs == 1 { "" } else { "s" },
                mean_rarity
            ),
        }.render()?));
    }

    // Build the supplied payload.  Stats row first so the model
    // anchors its analysis on the population context, then one row
    // per anomaly (capped at cfg.max_rows so a long-tail run can't
    // blow the prompt).  Stats row is tagged so its synthetic-row
    // nature is clear inside the prompt.
    let n_anomalies_fed = matched_anomalies.min(cfg.max_rows);
    let mut rows: Vec<JsonValue> = Vec::with_capacity(n_anomalies_fed + 1);
    rows.push(json!({
        "_kind":             "anomaly_window_stats",
        "n_logs":            n_logs,
        "n_unique_ngrams":   n_unique_ngrams,
        "n_grams":           n_eff,
        "anomaly_threshold": threshold_used,
        "mean_rarity":       mean_rarity,
        "duration":          p.duration,
        "n_anomalies_total": matched_anomalies,
        "n_anomalies_fed":   n_anomalies_fed,
    }));
    for a in anomalies_raw.iter().take(n_anomalies_fed) {
        let mut obj = a.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("_kind".into(), json!("anomaly"));
        }
        rows.push(obj);
    }

    let analyze_resp = signed_rpc_with_timeout(
        &state,
        "v4/llm.analyze",
        json!({
            "kind":            "supplied",
            "rows":            rows,
            // No `query` field — this page is detection-driven, not
            // query-driven; the RAG context IS the anomaly set.
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
            Ok(Html(AnomalyAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                n_anomalies_fed, matched_anomalies,
                n_logs, mean_rarity,
                anomaly_threshold: threshold_used,
                duration: p.duration,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(AnomalyAnalysis {
            response:          String::new(),
            response_html:     String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_anomalies_fed, matched_anomalies,
            n_logs, mean_rarity,
            anomaly_threshold: threshold_used,
            duration: p.duration,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
