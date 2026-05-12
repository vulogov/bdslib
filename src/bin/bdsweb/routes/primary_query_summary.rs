use askama::Template;
use axum::{extract::{Query, State}, response::Html};
use serde::Deserialize;
use serde_json::json;

use crate::{admin::signed_rpc, admin::signed_rpc_with_timeout, client::{mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub max_sentences: usize,
    #[serde(default = "default_min_word_len")]
    pub min_word_len: usize,
}
fn default_min_word_len() -> usize { 2 }

// ── Full page (shell) ─────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "primary_query_summary.html")]
struct PrimaryQuerySummaryPage {
    q:                String,
    max_sentences:    usize,
    min_word_len:     usize,
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
            log::warn!("[primary_query_summary] v4/llm.providers.list failed: {e}");
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
    Ok(Html(PrimaryQuerySummaryPage {
        q:             p.q,
        max_sentences: p.max_sentences,
        min_word_len:  p.min_word_len,
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/primary_query_summary_result.html")]
struct PrimaryQuerySummaryResult {
    q:             String,
    max_sentences: usize,
    summary:       String,
    has_summary:   bool,
    no_query:      bool,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    if p.q.trim().is_empty() {
        return Ok(Html(PrimaryQuerySummaryResult {
            q: p.q, max_sentences: p.max_sentences,
            summary: String::new(), has_summary: false, no_query: true,
        }.render()?));
    }

    let resp = rpc_versioned(&state, "v2/summary_for_query", "v3/summary_for_query", json!({
        "session":       SESSION,
        "query":         p.q,
        "max_sentences": p.max_sentences,
        "min_word_len":  p.min_word_len,
    })).await?;

    let summary = resp.get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let has_summary = !summary.is_empty();

    Ok(Html(PrimaryQuerySummaryResult {
        q:             p.q,
        max_sentences: p.max_sentences,
        summary,
        has_summary,
        no_query: false,
    }.render()?))
}

// ── HTMX: "Analyze this!" — answer-the-question via LLM ──────────────────────
//
// Mirror of `routes::primary_summary::analyze`, but the underlying
// RPC is `v?/summary_for_query` (query-focused TextRank) and the
// default prompt asks the model to *answer the operator's question*
// using the summary as evidence — rather than telling a general
// story.  Empty query → no analysis (the inline `results` handler
// already short-circuits there).

#[derive(Template)]
#[template(path = "partials/primary_query_summary_analysis.html")]
struct PrimaryQuerySummaryAnalysis {
    response:      String,
    response_html: String,
    provider:      String,
    model:         String,
    ms:            u64,
    /// Characters in the summary handed to the LLM — surfaced in
    /// the strip so the operator can tell when the summary was
    /// thin and the analysis is necessarily limited.
    summary_chars: usize,
    /// Echo of the search query for the panel header — this page
    /// is unique in being query-driven, so the operator wants to
    /// see what they searched for alongside the answer.
    q:             String,
    max_sentences: usize,
    min_word_len:  usize,
    /// `"miss"`, `"hit"`, or `""`.
    cache:         String,
    /// Empty when the LLM ran cleanly; banner message otherwise.
    error:         String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let cfg = state.primary_query_summary_analyze.clone();

    if p.q.trim().is_empty() {
        return Ok(Html(PrimaryQuerySummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars: 0,
            q:             p.q.clone(),
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            cache:    String::new(),
            error:    "Enter a query first — Primary Query Summary needs a vector \
                       search target to summarise.".to_owned(),
        }.render()?));
    }

    // Re-run the same RPC the inline `results` handler used so the
    // analysis is grounded in the exact text the operator is looking
    // at on the page (query + max_sentences + min_word_len all
    // shape the summary).
    let resp = match rpc_versioned(
        &state, "v2/summary_for_query", "v3/summary_for_query", json!({
            "session":       SESSION,
            "query":         p.q,
            "max_sentences": p.max_sentences,
            "min_word_len":  p.min_word_len,
        })).await
    {
        Ok(v)  => v,
        Err(e) => return Ok(Html(PrimaryQuerySummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars: 0,
            q:             p.q.clone(),
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            cache:    String::new(),
            error:    format!("Could not fetch query-focused TextRank summary for analysis: {e}"),
        }.render()?)),
    };
    let summary = resp.get("summary").and_then(|x| x.as_str())
        .unwrap_or("").trim().to_owned();
    let summary_chars = summary.chars().count();

    if summary.is_empty() {
        return Ok(Html(PrimaryQuerySummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars: 0,
            q:             p.q.clone(),
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            cache:    String::new(),
            error:    format!(
                "No text-bearing primary records matched the query `{}` strongly enough \
                 to produce a TextRank summary.  Try a broader query, or check that \
                 primary ingestion is emitting text bodies (numeric-only records are \
                 filtered out).",
                p.q
            ),
        }.render()?));
    }

    // Single-row supplied payload.  Carry the QUERY alongside the
    // summary — that's what makes this page different from
    // primary_summary, and the prompt's job is to answer the query
    // using the summary as evidence.  TextRank knobs are included
    // too so the model can calibrate confidence (a one-sentence
    // summary is a much more compressed signal than 20 sentences).
    let rows = vec![json!({
        "_kind":         "primary_query_summary",
        "query":         p.q,
        "summary":       summary,
        "max_sentences": p.max_sentences,
        "min_word_len":  p.min_word_len,
    })];

    let analyze_resp = signed_rpc_with_timeout(
        &state,
        "v4/llm.analyze",
        json!({
            "kind":            "supplied",
            "rows":            rows,
            // Pass the operator's query through so `v4/llm.analyze`
            // can compose it into the prompt as the question being
            // answered.  This is the distinguishing trait of this
            // target compared to primary_summary.
            "query":           p.q,
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
            Ok(Html(PrimaryQuerySummaryAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                summary_chars,
                q:             p.q,
                max_sentences: p.max_sentences,
                min_word_len:  p.min_word_len,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(PrimaryQuerySummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars,
            q:             p.q,
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
