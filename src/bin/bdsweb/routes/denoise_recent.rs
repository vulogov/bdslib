use askama::Template;
use axum::{extract::{Query, State}, response::Html};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

use crate::{admin::signed_rpc, admin::signed_rpc_with_timeout, client::{mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default = "default_n")]
    pub n: usize,
    #[serde(default = "default_min_word_len")]
    pub min_word_len: usize,
    #[serde(default = "default_noise_threshold")]
    pub noise_threshold: f32,
    #[serde(default = "default_max_kept")]
    pub max_kept: usize,
    #[serde(default = "default_max_removed")]
    pub max_removed: usize,
}
fn default_duration()        -> String { "1h".to_owned() }
fn default_n()               -> usize { 2 }
fn default_min_word_len()    -> usize { 2 }
fn default_noise_threshold() -> f32   { 0.85 }
fn default_max_kept()        -> usize { 100 }
fn default_max_removed()     -> usize { 100 }

// ── Page shell ────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "denoise_recent.html")]
struct DenoisePage {
    duration:         String,
    n:                usize,
    min_word_len:     usize,
    noise_threshold:  f32,
    max_kept:         usize,
    max_removed:      usize,
    mode_badge:       ModeBadge,
    /// Default LLM provider id, surfaced on `data-` for the
    /// wait-message JS.  Empty when bdsnode reports no providers.
    analyze_provider: String,
    /// Default model name.  Empty when unavailable.
    analyze_model:    String,
}

/// Fetch the default v4/llm provider + model so the wait-message JS
/// can name the actual upstream.  All failure modes collapse to
/// `("", "")` — the page falls back to generic phrasing.
async fn fetch_analyze_provider(state: &AppState) -> (String, String) {
    if state.shared_secret.is_empty() {
        return (String::new(), String::new());
    }
    let resp = match signed_rpc(state, "v4/llm.providers.list", json!({})).await {
        Ok(v)  => v,
        Err(e) => {
            log::warn!("[denoise_recent] v4/llm.providers.list failed: {e}");
            return (String::new(), String::new());
        }
    };
    let default_id = resp.get("default").and_then(|v| v.as_str()).unwrap_or("");
    if default_id.is_empty() { return (String::new(), String::new()); }
    let model = resp.get("providers").and_then(|v| v.as_array())
        .and_then(|arr| arr.iter().find(|p|
            p.get("id").and_then(|x| x.as_str()) == Some(default_id)))
        .and_then(|p| p.get("default_model").and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_owned();
    (default_id.to_owned(), model)
}

pub async fn page(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let mode_badge = mode_badge_for_page(&state, true).await;
    let (analyze_provider, analyze_model) = fetch_analyze_provider(&state).await;
    Ok(Html(DenoisePage {
        duration:        p.duration,
        n:               p.n,
        min_word_len:    p.min_word_len,
        noise_threshold: p.noise_threshold,
        max_kept:        p.max_kept,
        max_removed:     p.max_removed,
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Debug)]
pub struct DenoiseRow {
    pub idx:        u64,
    pub commonness: f64,
    pub text:       String,
}

#[derive(Template)]
#[template(path = "partials/denoise_recent_result.html")]
struct DenoiseResult {
    duration:        String,
    n_logs:          u64,
    n:               u64,
    n_unique_ngrams: u64,
    noise_threshold: f64,
    n_kept:          u64,
    n_removed:       u64,
    has_kept:        bool,
    has_removed:     bool,
    kept:            Vec<DenoiseRow>,
    removed:         Vec<DenoiseRow>,
    mode_badge:      ModeBadge,
}

fn rows(arr: Option<&Vec<JsonValue>>) -> Vec<DenoiseRow> {
    arr.map(|items| items.iter().map(|v| DenoiseRow {
        idx:        v.get("idx").and_then(JsonValue::as_u64).unwrap_or(0),
        commonness: v.get("commonness").and_then(JsonValue::as_f64).unwrap_or(0.0),
        text:       v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_owned(),
    }).collect()).unwrap_or_default()
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let resp = rpc_versioned(&state, "v2/denoise.recent", "v3/denoise.recent", json!({
        "session":         SESSION,
        "duration":        p.duration.clone(),
        "n":               p.n,
        "min_word_len":    p.min_word_len,
        "noise_threshold": p.noise_threshold,
        "max_kept":        p.max_kept,
        "max_removed":     p.max_removed,
    })).await?;

    let n_logs          = resp.get("n_logs").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_eff           = resp.get("n").and_then(JsonValue::as_u64).unwrap_or(p.n as u64);
    let n_unique_ngrams = resp.get("n_unique_ngrams").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_kept          = resp.get("n_kept").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_removed       = resp.get("n_removed").and_then(JsonValue::as_u64).unwrap_or(0);
    let noise_threshold = resp.get("noise_threshold").and_then(JsonValue::as_f64)
        .unwrap_or(p.noise_threshold as f64);

    let kept    = rows(resp.get("kept").and_then(JsonValue::as_array));
    let removed = rows(resp.get("removed").and_then(JsonValue::as_array));

    let has_kept    = !kept.is_empty();
    let has_removed = !removed.is_empty();
    let mode_badge  = ModeBadge::from_response(&resp);

    Ok(Html(DenoiseResult {
        duration:        p.duration,
        n_logs,
        n:               n_eff,
        n_unique_ngrams,
        noise_threshold,
        n_kept,
        n_removed,
        has_kept,
        has_removed,
        kept,
        removed,
        mode_badge,
    }.render()?))
}

// ── HTMX: "Analyze this!" — story-from-signal + filter sanity check ──────────
//
// Re-runs `v?/denoise.recent` with the same params, then ships a
// three-tier payload: one synthetic `_kind=denoise_window_stats` row,
// plus N kept + M removed rows tagged with their respective `_kind`.
// `cfg.max_rows` caps the total of kept + removed; **at least half the
// budget is reserved for kept** so a noisy window can't drown out the
// signal the LLM is supposed to be analysing.

/// Split `budget` row slots between two corpora with a 60/40 bias
/// in favour of `matched_a` (the signal side).  Slack from either
/// corpus is redistributed to the other so the full budget is
/// always used when there's enough supply.  Pure arithmetic; tested
/// indirectly via the analyze handler.
fn split_two_corpus_budget(
    matched_a: usize,
    matched_b: usize,
    budget:    usize,
) -> (usize, usize) {
    if budget == 0 { return (0, 0); }
    let total_matched = matched_a + matched_b;
    if total_matched <= budget {
        return (matched_a, matched_b);
    }
    let a_target = (budget * 6) / 10;
    let b_target = budget - a_target;
    let mut a = matched_a.min(a_target);
    let mut b = matched_b.min(b_target);
    let used = a + b;
    if used < budget {
        let leftover = budget - used;
        let a_extra = leftover.min(matched_a.saturating_sub(a));
        a += a_extra;
        let leftover2 = (budget - a - b).min(matched_b.saturating_sub(b));
        b += leftover2;
    }
    (a, b)
}

#[derive(Template)]
#[template(path = "partials/denoise_recent_analysis.html")]
struct DenoiseAnalysis {
    response:        String,
    response_html:   String,
    provider:        String,
    model:           String,
    ms:              u64,
    /// Kept rows fed to the LLM after the budget split.
    n_kept_fed:      usize,
    /// Removed rows fed to the LLM after the budget split.
    n_removed_fed:   usize,
    /// Total kept rows returned by v?/denoise.recent.
    matched_kept:    usize,
    /// Total removed rows returned by v?/denoise.recent.
    matched_removed: usize,
    n_logs:          u64,
    noise_threshold: f64,
    duration:        String,
    /// `"miss"`, `"hit"`, or `""`.
    cache:           String,
    /// Empty when the LLM ran cleanly; banner message otherwise.
    error:           String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let cfg = state.denoise_recent_analyze.clone();

    let resp = match rpc_versioned(&state, "v2/denoise.recent", "v3/denoise.recent", json!({
        "session":         SESSION,
        "duration":        p.duration.clone(),
        "n":               p.n,
        "min_word_len":    p.min_word_len,
        "noise_threshold": p.noise_threshold,
        "max_kept":        p.max_kept,
        "max_removed":     p.max_removed,
    })).await {
        Ok(v)  => v,
        Err(e) => return Ok(Html(DenoiseAnalysis {
            response:        String::new(),
            response_html:   String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:              0,
            n_kept_fed:      0,
            n_removed_fed:   0,
            matched_kept:    0,
            matched_removed: 0,
            n_logs:          0,
            noise_threshold: p.noise_threshold as f64,
            duration:        p.duration.clone(),
            cache:    String::new(),
            error:    format!("Could not fetch denoise results for analysis: {e}"),
        }.render()?)),
    };

    let n_logs           = resp.get("n_logs").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_eff            = resp.get("n").and_then(JsonValue::as_u64).unwrap_or(p.n as u64);
    let n_unique_ngrams  = resp.get("n_unique_ngrams").and_then(JsonValue::as_u64).unwrap_or(0);
    let threshold_used   = resp.get("noise_threshold").and_then(JsonValue::as_f64)
        .unwrap_or(p.noise_threshold as f64);

    let kept_raw:    Vec<JsonValue> = resp.get("kept")
        .and_then(JsonValue::as_array).cloned().unwrap_or_default();
    let removed_raw: Vec<JsonValue> = resp.get("removed")
        .and_then(JsonValue::as_array).cloned().unwrap_or_default();
    let matched_kept    = kept_raw.len();
    let matched_removed = removed_raw.len();

    if matched_kept == 0 && matched_removed == 0 {
        return Ok(Html(DenoiseAnalysis {
            response:        String::new(),
            response_html:   String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:              0,
            n_kept_fed:      0,
            n_removed_fed:   0,
            matched_kept:    0,
            matched_removed: 0,
            n_logs,
            noise_threshold: threshold_used,
            duration:        p.duration.clone(),
            cache:    String::new(),
            error:    format!(
                "No records scanned in the last {} (corpus empty) — widen the duration \
                 or check that primary ingestion is running.",
                p.duration
            ),
        }.render()?));
    }

    // Two-corpus budget split, 60/40 in favour of kept (the signal —
    // the whole point of the analysis).  Slack from either side
    // spills to the other so we never under-use the budget when one
    // corpus is small.  Example: max_rows=50, 100 kept, 200 removed
    // → 30 kept + 20 removed.  Example: max_rows=50, 3 kept, 200
    // removed → 3 kept + 47 removed (kept under-used, slack to
    // removed).
    let (n_kept_fed, n_removed_fed) = split_two_corpus_budget(
        matched_kept, matched_removed, cfg.max_rows,
    );

    // Build the payload.  Stats row first so the model anchors on
    // the population context.  Kept rows next, then removed rows.
    let mut rows: Vec<JsonValue> = Vec::with_capacity(n_kept_fed + n_removed_fed + 1);
    rows.push(json!({
        "_kind":           "denoise_window_stats",
        "n_logs":          n_logs,
        "n_unique_ngrams": n_unique_ngrams,
        "n_grams":         n_eff,
        "noise_threshold": threshold_used,
        "n_kept_total":    matched_kept,
        "n_removed_total": matched_removed,
        "n_kept_fed":      n_kept_fed,
        "n_removed_fed":   n_removed_fed,
        "duration":        p.duration,
    }));
    for r in kept_raw.iter().take(n_kept_fed) {
        let mut obj = r.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("_kind".into(), json!("denoise_kept"));
        }
        rows.push(obj);
    }
    for r in removed_raw.iter().take(n_removed_fed) {
        let mut obj = r.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("_kind".into(), json!("denoise_removed"));
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
            Ok(Html(DenoiseAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                n_kept_fed, n_removed_fed,
                matched_kept, matched_removed,
                n_logs,
                noise_threshold: threshold_used,
                duration: p.duration,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(DenoiseAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_kept_fed, n_removed_fed,
            matched_kept, matched_removed,
            n_logs,
            noise_threshold: threshold_used,
            duration: p.duration,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
