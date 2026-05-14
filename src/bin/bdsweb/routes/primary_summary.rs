use askama::Template;
use axum::{extract::{Form, Query, State}, response::Html};
use serde::Deserialize;
use serde_json::json;

use crate::{admin::signed_rpc_with_timeout, client::{mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default)]
    pub max_sentences: usize,
    #[serde(default = "default_min_word_len")]
    pub min_word_len: usize,
}
fn default_duration()    -> String { "1h".to_owned() }
fn default_min_word_len() -> usize { 2 }

// ── Full page (shell) ─────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "primary_summary.html")]
struct PrimarySummaryPage {
    duration:         String,
    max_sentences:    usize,
    min_word_len:     usize,
    mode_badge:       ModeBadge,
    /// Default LLM provider id, surfaced on `data-` for the
    /// wait-message JS.  Empty when bdsnode reports no providers.
    analyze_provider: String,
    /// Default model name.  Empty when unavailable.
    analyze_model:    String,
}

pub async fn page(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let (mode_badge, (analyze_provider, analyze_model)) = tokio::join!(
        mode_badge_for_page(&state, true),
        crate::client::analyze_provider(&state),
    );
    Ok(Html(PrimarySummaryPage {
        duration:      p.duration,
        max_sentences: p.max_sentences,
        min_word_len:  p.min_word_len,
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/primary_summary_result.html")]
struct PrimarySummaryResult {
    duration:      String,
    max_sentences: usize,
    summary:       String,
    has_summary:   bool,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let resp = rpc_versioned(&state, "v2/summary_for_recent", "v3/summary_for_recent", json!({
        "session":       SESSION,
        "duration":      p.duration,
        "max_sentences": p.max_sentences,
        "min_word_len":  p.min_word_len,
    })).await?;

    let summary = resp.get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let has_summary = !summary.is_empty();

    Ok(Html(PrimarySummaryResult {
        duration:      p.duration,
        max_sentences: p.max_sentences,
        summary,
        has_summary,
    }.render()?))
}

// ── HTMX: "Analyze this!" — story-from-summary on primary records ─────────────
//
// The Primary Summary page displays a single TextRank-PageRank
// extract over text-bearing primary telemetry records.  Unlike the
// Templates Summary "Analyze this!", which also pulls topic
// keywords, this handler trusts the summary alone — the user
// explicitly asked for analysis "of the TextRank produced summary"
// using the "discovered summary to tell the story".  So we re-run
// the same `v?/summary_for_recent` call the page used (same params)
// and hand the summary verbatim to `v4/llm.analyze` as a single
// `_kind=primary_summary` row.

#[derive(Template)]
#[template(path = "partials/primary_summary_analysis.html")]
struct PrimarySummaryAnalysis {
    response:      String,
    response_html: String,
    provider:      String,
    model:         String,
    ms:            u64,
    /// Characters in the summary handed to the LLM — surfaced in
    /// the strip so the operator can tell at a glance when the
    /// input was sparse and the analysis is necessarily thin.
    summary_chars: usize,
    duration:      String,
    max_sentences: usize,
    min_word_len:  usize,
    /// `"miss"`, `"hit"`, or `""`.
    cache:         String,
    /// Empty when the LLM ran cleanly; banner message otherwise.
    error:         String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Form(p): Form<Params>,
) -> Result<Html<String>, AppError> {
    let cfg = state.primary_summary_analyze.clone();

    // Re-run the same RPC the inline `results` handler used so the
    // analysis is grounded in the exact text the operator is
    // looking at on the page (max_sentences + min_word_len shape
    // the summary itself).
    let resp = match rpc_versioned(
        &state, "v2/summary_for_recent", "v3/summary_for_recent", json!({
            "session":       SESSION,
            "duration":      p.duration,
            "max_sentences": p.max_sentences,
            "min_word_len":  p.min_word_len,
        })).await
    {
        Ok(v)  => v,
        Err(e) => return Ok(Html(PrimarySummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars: 0,
            duration:      p.duration.clone(),
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            cache:    String::new(),
            error:    format!("Could not fetch TextRank summary for analysis: {e}"),
        }.render()?)),
    };
    let summary = resp.get("summary").and_then(|x| x.as_str())
        .unwrap_or("").trim().to_owned();
    let summary_chars = summary.chars().count();

    if summary.is_empty() {
        return Ok(Html(PrimarySummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars: 0,
            duration:      p.duration.clone(),
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            cache:    String::new(),
            error:    format!(
                "No text-bearing primary records in the last {} produced a TextRank \
                 summary — widen the duration or check that primary ingestion is \
                 emitting text bodies (numeric-only records are filtered out).",
                p.duration
            ),
        }.render()?));
    }

    // Single-row supplied payload.  Carrying the TextRank knobs
    // alongside the summary lets the model calibrate confidence
    // (a one-sentence summary from `max_sentences=1` is a much
    // more compressed signal than a 20-sentence summary).
    let rows = vec![json!({
        "_kind":         "primary_summary",
        "summary":       summary,
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
            Ok(Html(PrimarySummaryAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                summary_chars,
                duration:      p.duration,
                max_sentences: p.max_sentences,
                min_word_len:  p.min_word_len,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(PrimarySummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars,
            duration:      p.duration,
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
