use askama::Template;
use axum::{extract::{Form, Query, State}, response::Html};
use serde::Deserialize;
use serde_json::json;

use crate::{admin::signed_rpc_with_timeout, client::{mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub max_sentences: usize,
    #[serde(default = "default_min_word_len")]
    pub min_word_len: usize,
    #[serde(default = "default_n_concepts")]
    pub n_concepts: usize,
}
fn default_min_word_len() -> usize { 2 }
fn default_n_concepts()   -> usize { 3 }

// ── Full page (shell) ─────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "primary_lsa_query_summary.html")]
struct PrimaryLsaQuerySummaryPage {
    q:                String,
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

pub async fn page(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let (mode_badge, (analyze_provider, analyze_model)) = tokio::join!(
        mode_badge_for_page(&state, true),
        crate::client::analyze_provider(&state),
    );
    Ok(Html(PrimaryLsaQuerySummaryPage {
        q:             p.q,
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
#[template(path = "partials/primary_lsa_query_summary_result.html")]
struct PrimaryLsaQuerySummaryResult {
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
        return Ok(Html(PrimaryLsaQuerySummaryResult {
            q: p.q, max_sentences: p.max_sentences,
            summary: String::new(), has_summary: false, no_query: true,
        }.render()?));
    }

    let resp = rpc_versioned(&state, "v2/summary_lsa_for_query", "v3/summary_lsa_for_query", json!({
        "session":       SESSION,
        "query":         p.q,
        "max_sentences": p.max_sentences,
        "min_word_len":  p.min_word_len,
        "n_concepts":    p.n_concepts,
    })).await?;

    let summary = resp.get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let has_summary = !summary.is_empty();

    Ok(Html(PrimaryLsaQuerySummaryResult {
        q:             p.q,
        max_sentences: p.max_sentences,
        summary,
        has_summary,
        no_query: false,
    }.render()?))
}

// ── HTMX: "Analyze this!" — answer-the-question, per-concept ─────────────────
//
// Mirror of `routes::primary_lsa_summary::analyze`, but the RPC is
// `v?/summary_lsa_for_query` (query-driven LSA) and the prompt asks
// the model to answer the operator's question by reading the
// per-concept structure of the summary — combining the
// answer-the-question semantics of primary_query_summary with the
// per-concept breakdown of primary_lsa_summary.

#[derive(Template)]
#[template(path = "partials/primary_lsa_query_summary_analysis.html")]
struct PrimaryLsaQuerySummaryAnalysis {
    response:      String,
    response_html: String,
    provider:      String,
    model:         String,
    ms:            u64,
    /// Characters in the summary handed to the LLM.
    summary_chars: usize,
    /// Echo of the operator's query — this page is query-driven so
    /// the echo is essential for context.
    q:             String,
    max_sentences: usize,
    min_word_len:  usize,
    /// Number of LSA concept dimensions the operator picked.
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
    let cfg = state.primary_lsa_query_summary_analyze.clone();

    if p.q.trim().is_empty() {
        return Ok(Html(PrimaryLsaQuerySummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars: 0,
            q:             p.q.clone(),
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            n_concepts:    p.n_concepts,
            cache:    String::new(),
            error:    "Enter a query first — Primary LSA Query Summary needs a vector \
                       search target to summarise.".to_owned(),
        }.render()?));
    }

    // Re-run the same RPC the inline `results` handler used so the
    // analysis is grounded in the exact text the operator is looking
    // at on the page (query + max_sentences + min_word_len +
    // n_concepts all shape the summary).
    let resp = match rpc_versioned(
        &state, "v2/summary_lsa_for_query", "v3/summary_lsa_for_query", json!({
            "session":       SESSION,
            "query":         p.q,
            "max_sentences": p.max_sentences,
            "min_word_len":  p.min_word_len,
            "n_concepts":    p.n_concepts,
        })).await
    {
        Ok(v)  => v,
        Err(e) => return Ok(Html(PrimaryLsaQuerySummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars: 0,
            q:             p.q.clone(),
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            n_concepts:    p.n_concepts,
            cache:    String::new(),
            error:    format!("Could not fetch query-focused LSA summary for analysis: {e}"),
        }.render()?)),
    };
    let summary = resp.get("summary").and_then(|x| x.as_str())
        .unwrap_or("").trim().to_owned();
    let summary_chars = summary.chars().count();

    if summary.is_empty() {
        return Ok(Html(PrimaryLsaQuerySummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars: 0,
            q:             p.q.clone(),
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            n_concepts:    p.n_concepts,
            cache:    String::new(),
            error:    format!(
                "No text-bearing primary records matched the query `{}` strongly enough \
                 for LSA to produce a summary — try a broader query, lower `n_concepts`, \
                 or check that primary ingestion is emitting text bodies.",
                p.q
            ),
        }.render()?));
    }

    // Single-row supplied payload.  Carry the query, summary, and
    // both LSA + TextRank knobs.  `query` is also passed as a
    // top-level v4/llm.analyze param so the inference cache key is
    // query-aware (different queries against the same window
    // produce different cache entries).
    let rows = vec![json!({
        "_kind":         "primary_lsa_query_summary",
        "query":         p.q,
        "summary":       summary,
        "n_concepts":    p.n_concepts,
        "max_sentences": p.max_sentences,
        "min_word_len":  p.min_word_len,
    })];

    let analyze_resp = signed_rpc_with_timeout(
        &state,
        "v4/llm.analyze",
        json!({
            "kind":            "supplied",
            "rows":            rows,
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
            Ok(Html(PrimaryLsaQuerySummaryAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                summary_chars,
                q:             p.q,
                max_sentences: p.max_sentences,
                min_word_len:  p.min_word_len,
                n_concepts:    p.n_concepts,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(PrimaryLsaQuerySummaryAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            summary_chars,
            q:             p.q,
            max_sentences: p.max_sentences,
            min_word_len:  p.min_word_len,
            n_concepts:    p.n_concepts,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
