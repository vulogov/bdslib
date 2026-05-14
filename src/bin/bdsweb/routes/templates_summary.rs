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
    /// `0` → derive summary length from `ratio` server-side.
    #[serde(default)]
    pub max_sentences: usize,
    #[serde(default = "default_min_word_len")]
    pub min_word_len: usize,
}
fn default_duration()    -> String { "1h".to_owned() }
fn default_min_word_len() -> usize { 2 }

// ── Full page (shell) ─────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "templates_summary.html")]
struct TemplatesSummaryPage {
    duration:         String,
    max_sentences:    usize,
    min_word_len:     usize,
    mode_badge:       ModeBadge,
    /// Default LLM provider id; surfaced on a `data-` attribute so
    /// the wait-message JS can name the actual upstream.  Empty
    /// when bdsnode reports no providers registered.
    analyze_provider: String,
    /// Default model name for that provider.  Empty when unavailable.
    analyze_model:    String,
}

/// Fetch the default v4/llm provider + model so the wait-message JS
/// can name the actual upstream.  All failure modes collapse to
/// `("", "")` and the page falls back to generic phrasing.
async fn fetch_analyze_provider(state: &AppState) -> (String, String) {
    if state.shared_secret.is_empty() {
        return (String::new(), String::new());
    }
    let resp = match signed_rpc(state, "v4/llm.providers.list", json!({})).await {
        Ok(v)  => v,
        Err(e) => {
            log::warn!("[templates_summary] v4/llm.providers.list failed: {e}");
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
    Ok(Html(TemplatesSummaryPage {
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
#[template(path = "partials/templates_summary_result.html")]
struct TemplatesSummaryResult {
    duration:      String,
    max_sentences: usize,
    summary:       String,
    has_summary:   bool,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let resp = rpc_versioned(&state, "v2/textrank.templates", "v3/textrank.templates", json!({
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

    Ok(Html(TemplatesSummaryResult {
        duration:      p.duration,
        max_sentences: p.max_sentences,
        summary,
        has_summary,
    }.render()?))
}

// ── HTMX: "Analyze this!" — story-from-summary via LLM ───────────────────────
//
// The Templates Summary page is structurally unique among the
// "Analyze this!" buttons.  The page shows a single derived blob (the
// TextRank summary string) rather than a row list, and the value of
// the LLM step is *weaving that summary together with the
// LDA-discovered topic keywords into a coherent story*.  So we call
// two RPCs server-side — `v?/textrank.templates` (same params the
// page used) for the summary, and `v?/topics.all` for the keywords —
// then tag each piece (`_kind=textrank_summary` /
// `_kind=topic_keywords`) before handing to v4/llm.analyze.

#[derive(Template)]
#[template(path = "partials/templates_summary_analysis.html")]
struct TemplatesSummaryAnalysis {
    response:        String,
    response_html:   String,
    provider:        String,
    model:           String,
    ms:              u64,
    /// Number of characters in the TextRank summary fed to the LLM.
    /// Shown in the strip so the operator can tell when the summary
    /// itself was empty / sparse and the analysis is keyword-only.
    summary_chars:   usize,
    /// Number of (key → keywords[]) rows from `v?/topics.all` after
    /// the `max_rows` cap.  Cap is applied to the topics rows only —
    /// the summary always gets through (one row, always).
    n_topics:        usize,
    /// Topics returned by `v?/topics.all` before the cap, so the
    /// strip can show "8/14 topics" when sampling kicked in.
    matched_topics:  usize,
    duration:        String,
    /// `"miss"`, `"hit"`, or `""`.
    cache:           String,
    /// Empty when the LLM ran cleanly; banner message otherwise.
    error:           String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Form(p): Form<Params>,
) -> Result<Html<String>, AppError> {
    let cfg = state.templates_summary_analyze.clone();

    // Pull the TextRank summary the page just rendered.  Same params
    // as the inline `results` handler so the analysis sees the same
    // sentence cap / min-word-length the operator picked.
    let summary_resp = match rpc_versioned(
        &state, "v2/textrank.templates", "v3/textrank.templates", json!({
            "session":       SESSION,
            "duration":      p.duration,
            "max_sentences": p.max_sentences,
            "min_word_len":  p.min_word_len,
        })).await
    {
        Ok(v)  => v,
        Err(e) => return Ok(Html(TemplatesSummaryAnalysis {
            response:        String::new(),
            response_html:   String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:              0,
            summary_chars:   0,
            n_topics:        0,
            matched_topics:  0,
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    format!("Could not fetch TextRank summary for analysis: {e}"),
        }.render()?)),
    };
    let summary = summary_resp.get("summary").and_then(|x| x.as_str())
        .unwrap_or("").trim().to_owned();
    let summary_chars = summary.chars().count();

    // Pull the LDA topic keywords for the same window.  Failures
    // here are NOT fatal — we can still hand the summary to the LLM
    // and the operator gets a thinner but useful analysis.
    let topics_resp = rpc_versioned(
        &state, "v2/topics.all", "v3/topics.all", json!({
            "session":  SESSION,
            "duration": p.duration,
        })).await;
    let topics_raw: Vec<serde_json::Value> = match &topics_resp {
        Ok(v)  => v.get("topics").and_then(|x| x.as_array()).cloned().unwrap_or_default(),
        Err(e) => {
            log::warn!("[templates_summary] v?/topics.all failed (continuing with summary only): {e}");
            Vec::new()
        }
    };
    let matched_topics = topics_raw.len();

    // If we have neither a summary nor any topics, there's literally
    // nothing for the LLM to chew on — render a friendly banner.
    if summary.is_empty() && matched_topics == 0 {
        return Ok(Html(TemplatesSummaryAnalysis {
            response:        String::new(),
            response_html:   String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:              0,
            summary_chars:   0,
            n_topics:        0,
            matched_topics:  0,
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    format!(
                "No TextRank summary and no topic keywords for the last {} — \
                 drain3 hasn't mined enough templates yet, or widen the duration.",
                p.duration
            ),
        }.render()?));
    }

    // Build the supplied payload: one summary row + N topic rows.
    // The summary always wins a slot; topics get the remainder of
    // `cfg.max_rows`.  Many deployments have <20 keys so this rarely
    // matters in practice, but the cap stays here so a 10k-key
    // cluster doesn't blow the prompt budget.
    let topic_budget = cfg.max_rows.saturating_sub(1);
    let n_topics    = topics_raw.len().min(topic_budget);

    let mut rows: Vec<serde_json::Value> = Vec::with_capacity(n_topics + 1);
    if !summary.is_empty() {
        rows.push(json!({
            "_kind":           "textrank_summary",
            "summary":         summary,
            "max_sentences":   p.max_sentences,
            "min_word_len":    p.min_word_len,
        }));
    }
    for t in topics_raw.iter().take(n_topics) {
        // `v?/topics.all` returns {key, keywords: "comma,separated"}
        // — split into a JSON array so the model sees each token as
        // its own item rather than one quoted string.
        let key = t.get("key").and_then(|x| x.as_str()).unwrap_or("").to_owned();
        let kw_str = t.get("keywords").and_then(|x| x.as_str()).unwrap_or("");
        let keywords: Vec<String> = kw_str.split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        rows.push(json!({
            "_kind":    "topic_keywords",
            "key":      key,
            "keywords": keywords,
        }));
    }

    // Hand off to v4/llm.analyze (HMAC-signed).  Timeout + prompt
    // template come from `web.analyze.templates_summary.*` in hjson.
    let analyze_resp = signed_rpc_with_timeout(
        &state,
        "v4/llm.analyze",
        json!({
            "kind":            "supplied",
            "rows":            rows,
            // No "query" field — this page is not query-driven; the
            // RAG context IS the summary + keywords.
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
            Ok(Html(TemplatesSummaryAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                summary_chars,
                n_topics, matched_topics,
                duration: p.duration,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(TemplatesSummaryAnalysis {
            response:        String::new(),
            response_html:   String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:              0,
            summary_chars,
            n_topics, matched_topics,
            duration: p.duration,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
