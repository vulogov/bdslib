use askama::Template;
use axum::{extract::{Form, Query, State}, response::Html};
use serde::Deserialize;
use serde_json::json;

use crate::{admin::signed_rpc, admin::signed_rpc_with_timeout, client::{mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default)]
    pub max_sentences: usize,
    #[serde(default = "default_min_word_len")]
    pub min_word_len: usize,
    #[serde(default = "default_n_concepts")]
    pub n_concepts: usize,
}
fn default_duration()    -> String { "1h".to_owned() }
fn default_min_word_len() -> usize { 2 }
fn default_n_concepts()   -> usize { 3 }

// ── Full page (shell) ─────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "primary_lsa_summary.html")]
struct PrimaryLsaSummaryPage {
    duration:         String,
    max_sentences:    usize,
    min_word_len:     usize,
    n_concepts:       usize,
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
            log::warn!("[primary_lsa_summary] v4/llm.providers.list failed: {e}");
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
    Ok(Html(PrimaryLsaSummaryPage {
        duration:      p.duration,
        max_sentences: p.max_sentences,
        min_word_len:  p.min_word_len,
        n_concepts:    p.n_concepts,
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/primary_lsa_summary_result.html")]
struct PrimaryLsaSummaryResult {
    duration:      String,
    max_sentences: usize,
    summary:       String,
    has_summary:   bool,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let resp = rpc_versioned(&state, "v2/summary_lsa_for_recent", "v3/summary_lsa_for_recent", json!({
        "session":       SESSION,
        "duration":      p.duration,
        "max_sentences": p.max_sentences,
        "min_word_len":  p.min_word_len,
        "n_concepts":    p.n_concepts,
    })).await?;

    let summary = resp.get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let has_summary = !summary.is_empty();

    Ok(Html(PrimaryLsaSummaryResult {
        duration:      p.duration,
        max_sentences: p.max_sentences,
        summary,
        has_summary,
    }.render()?))
}

// ── HTMX: "Analyze this!" — per-concept interpretation via LLM ───────────────
//
// Mirror of `routes::primary_summary::analyze`, but the underlying
// RPC is `v?/summary_lsa_for_recent` (LSA-via-SVD concept
// decomposition) and the default prompt asks the model to
// interpret each concept dimension and look for cross-concept
// correlation — the value LSA adds over TextRank.

#[derive(Template)]
#[template(path = "partials/primary_lsa_summary_analysis.html")]
struct PrimaryLsaSummaryAnalysis {
    response:      String,
    response_html: String,
    provider:      String,
    model:         String,
    ms:            u64,
    /// Characters in the summary handed to the LLM.
    summary_chars: usize,
    duration:      String,
    max_sentences: usize,
    min_word_len:  usize,
    /// Number of LSA concept dimensions the operator picked — the
    /// strip shows this so the operator knows how many distinct
    /// threads the model was looking at.
    n_concepts:    usize,
    /// `"miss"`, `"hit"`, or `""`.
    cache:         String,
    /// Empty when the LLM ran cleanly; banner message otherwise.
    error:         String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Form(p): Form<Params>,
) -> Result<Html<String>, AppError> {
    let cfg = state.primary_lsa_summary_analyze.clone();

    // Re-run the same RPC the inline `results` handler used so the
    // analysis is grounded in the exact text the operator is
    // looking at (duration + max_sentences + min_word_len +
    // n_concepts all shape the summary).
    let resp = match rpc_versioned(
        &state, "v2/summary_lsa_for_recent", "v3/summary_lsa_for_recent", json!({
            "session":       SESSION,
            "duration":      p.duration,
            "max_sentences": p.max_sentences,
            "min_word_len":  p.min_word_len,
            "n_concepts":    p.n_concepts,
        })).await
    {
        Ok(v)  => v,
        Err(e) => return Ok(Html(PrimaryLsaSummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars: 0,
            duration:      p.duration.clone(),
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            n_concepts:    p.n_concepts,
            cache:    String::new(),
            error:    format!("Could not fetch LSA summary for analysis: {e}"),
        }.render()?)),
    };
    let summary = resp.get("summary").and_then(|x| x.as_str())
        .unwrap_or("").trim().to_owned();
    let summary_chars = summary.chars().count();

    if summary.is_empty() {
        return Ok(Html(PrimaryLsaSummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars: 0,
            duration:      p.duration.clone(),
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            n_concepts:    p.n_concepts,
            cache:    String::new(),
            error:    format!(
                "No text-bearing primary records in the last {} produced an LSA \
                 summary — widen the duration, lower `n_concepts`, or check that \
                 primary ingestion is emitting text bodies (LSA needs a non-trivial \
                 term-document matrix to decompose).",
                p.duration
            ),
        }.render()?));
    }

    // Single-row supplied payload.  Carry the LSA knob `n_concepts`
    // alongside the standard TextRank ones — the prompt treats each
    // summary sentence as one concept thread, so the model needs to
    // know how many threads to expect.
    let rows = vec![json!({
        "_kind":         "primary_lsa_summary",
        "summary":       summary,
        "n_concepts":    p.n_concepts,
        "max_sentences": p.max_sentences,
        "min_word_len":  p.min_word_len,
        "duration":      p.duration,
    })];

    let analyze_resp = signed_rpc_with_timeout(
        &state,
        "v4/llm.analyze",
        json!({
            "kind":            "supplied",
            "rows":            rows,
            // No `query` field — the summary IS the input.
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
            Ok(Html(PrimaryLsaSummaryAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                summary_chars,
                duration:      p.duration,
                max_sentences: p.max_sentences,
                min_word_len:  p.min_word_len,
                n_concepts:    p.n_concepts,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(PrimaryLsaSummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars,
            duration:      p.duration,
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            n_concepts:    p.n_concepts,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
