use askama::Template;
use axum::{extract::{Query, State}, response::Html};
use serde::Deserialize;
use serde_json::json;

use crate::{admin::signed_rpc_with_timeout, client::{fmt_ts, mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default)]
    pub q: String,
}
fn default_duration() -> String { "1h".to_owned() }

#[derive(Debug)]
pub struct LogRow {
    pub timestamp:         String,
    pub key:               String,
    pub message:           String,
    pub score:             String,
    pub secondaries_count: usize,
    pub secondaries_json:  String,
}

fn to_rows(results: &serde_json::Value) -> Vec<LogRow> {
    results.as_array()
           .map(|arr| arr.iter().map(hit_to_row).collect())
           .unwrap_or_default()
}

fn hit_to_row(v: &serde_json::Value) -> LogRow {
    let ts   = v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
    let data = v.get("data");
    let message = data
        .and_then(|d| d.as_str()).map(str::to_owned)
        .or_else(|| data.and_then(|d| d.get("message")).and_then(|m| m.as_str()).map(str::to_owned))
        .or_else(|| data.map(|d| d.to_string()))
        .unwrap_or_default();
    let score = v.get("_score").and_then(|x| x.as_f64())
                 .map(|f| format!("{f:.3}"))
                 .unwrap_or_else(|| "—".to_owned());
    let secs = v.get("secondaries").and_then(|x| x.as_array());
    let secondaries_count = secs.map(|a| a.len()).unwrap_or(0);
    let secondaries_json = secs
        .map(|a| serde_json::to_string(a).unwrap_or_else(|_| "[]".to_owned()))
        .unwrap_or_else(|| "[]".to_owned());
    LogRow {
        timestamp: fmt_ts(ts),
        key:       v.get("key").and_then(|x| x.as_str()).unwrap_or("—").to_owned(),
        message:   truncate(&message, 160),
        score,
        secondaries_count,
        secondaries_json,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_owned() } else { format!("{}…", &s[..n]) }
}

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "logs.html")]
struct LogsPage {
    duration:        String,
    q:               String,
    mode_badge:      ModeBadge,
    /// Default LLM provider id for "Analyze this!" (`"ollama"`,
    /// `"deepseek"`, `"anthropic"`, …).  Empty when bdsnode reports
    /// no providers registered.  Surfaced on a `data-` attribute so
    /// the wait-message JS can name the actual provider rather than
    /// the hardcoded "Ollama" it used to print.
    analyze_provider: String,
    /// Default model name for the same provider — included in the
    /// wait message after a `/`.  Empty when unavailable.
    analyze_model:    String,
}

/// Fetch the default v4/llm provider + its default model so the page
/// can label the "Analyze this!" wait message with the actual
/// upstream that will run the inference.  All failure modes
/// (missing HMAC secret, bdsnode unreachable, empty providers list,
/// configured `default` not registered) collapse to `("", "")` —
/// the page falls back to a generic "Asking the LLM…" string.
async fn fetch_analyze_provider(state: &AppState) -> (String, String) {
    if state.shared_secret.is_empty() {
        return (String::new(), String::new());
    }
    let resp = match crate::admin::signed_rpc(
        state, "v4/llm.providers.list", json!({})).await
    {
        Ok(v)  => v,
        Err(e) => {
            log::warn!("[logs] v4/llm.providers.list failed: {e}");
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
    Ok(Html(LogsPage {
        duration: p.duration,
        q:        p.q,
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX: key cloud ──────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/key_cloud.html")]
struct KeyCloud {
    keys:      Vec<String>,
    duration:  String,
    href_base: String,
}

pub async fn keys(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let resp = rpc_versioned(&state, "v2/keys.all", "v3/keys.all", json!({
        "session":  SESSION,
        "duration": p.duration,
        "key":      "*",
    })).await.unwrap_or_default();

    let keys = resp.get("keys")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();

    Ok(Html(KeyCloud {
        keys,
        duration:  p.duration,
        href_base: "/logs".to_owned(),
    }.render()?))
}

// ── HTMX: topics cloud ───────────────────────────────────────────────────────

pub struct TopicRow {
    pub key:      String,
    pub keywords: Vec<String>,
}

#[derive(Template)]
#[template(path = "partials/topics_cloud.html")]
struct TopicsCloud { topics: Vec<TopicRow>, duration: String }

pub async fn topics(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let resp = rpc_versioned(&state, "v2/topics.all", "v3/topics.all", json!({
        "session":  SESSION,
        "duration": p.duration,
    })).await;

    let topics = match resp {
        Err(_) => vec![],
        Ok(v) => {
            v.get("topics")
             .and_then(|x| x.as_array())
             .map(|arr| arr.iter().map(|t| {
                 let key = t.get("key").and_then(|x| x.as_str()).unwrap_or("").to_owned();
                 let kw_str = t.get("keywords").and_then(|x| x.as_str()).unwrap_or("");
                 let keywords = kw_str.split(',')
                     .map(|s| s.trim().to_owned())
                     .filter(|s| !s.is_empty())
                     .collect();
                 TopicRow { key, keywords }
             }).collect())
             .unwrap_or_default()
        }
    };

    Ok(Html(TopicsCloud { topics, duration: p.duration }.render()?))
}

// ── HTMX: vector search results fragment ─────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/log_rows.html")]
struct LogRows {
    rows:       Vec<LogRow>,
    duration:   String,
    q:          String,
    mode_badge: Option<ModeBadge>,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    if p.q.is_empty() {
        return Ok(Html(LogRows {
            rows: vec![], duration: p.duration, q: p.q, mode_badge: None,
        }.render()?));
    }

    let resp = rpc_versioned(&state, "v2/search.get", "v3/search.get", json!({
        "session":  SESSION,
        "query":    p.q,
        "duration": p.duration,
        "limit":    50,
    })).await?;

    let mode_badge = Some(ModeBadge::from_response(&resp));

    Ok(Html(LogRows {
        rows:     to_rows(&resp["results"]),
        duration: p.duration,
        q:        p.q,
        mode_badge,
    }.render()?))
}

// ── HTMX: "Analyze this!" — one-shot LLM analysis of current results ─────────
//
// Re-runs the same `v?/search.get` the page just executed so the
// LLM analysis is built on EXACTLY what the operator sees, then
// pipes the raw hits into `v4/llm.analyze` with `kind=supplied`.
// This is intentionally one-shot (no chat session, no history) —
// the prompt asks the model to surface patterns, anomalies, and
// likely root causes in the current query window.

#[derive(Template)]
#[template(path = "partials/log_analysis.html")]
struct LogAnalysis {
    /// Raw Markdown body — kept around for the empty-check.  The
    /// template renders [`response_html`] (a server-side HTML
    /// conversion of this string) instead.
    response:        String,
    /// HTML-rendered Markdown ready to interpolate with `|safe`.
    response_html:   String,
    /// Provider id (`ollama`, `anthropic`, …).
    provider:        String,
    /// Model name actually used.
    model:           String,
    /// Round-trip ms reported by the analyze endpoint.
    ms:              u64,
    /// How many rows fed into the prompt.
    n_rows:          usize,
    /// Echo of the search query for the panel header.
    q:               String,
    /// Echo of the duration for the panel header.
    duration:        String,
    /// One of: `"miss"`, `"hit"`, `"disabled"`.  Surfaced so a
    /// returning operator can tell when the LLM didn't actually run.
    cache:           String,
    /// Empty when the LLM ran cleanly; populated with a short error
    /// banner when the route caught a v4/v2 failure but still wants
    /// to render the panel.
    error:           String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    // No query → nothing to analyze; render a friendly hint instead
    // of an empty pane.
    if p.q.is_empty() {
        return Ok(Html(LogAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows:   0,
            q:        p.q.clone(),
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    "Run a search first — there are no logs to analyze yet.".to_owned(),
        }.render()?));
    }

    let cfg = state.logs_analyze.clone();

    // Re-run the same search the page just executed so the LLM sees
    // exactly what the operator sees.  Errors here are not fatal —
    // we'll show the failure in the panel rather than 500.  The row
    // limit is operator-configurable via `web.logs.analyze.max_rows`.
    let search = match rpc_versioned(&state, "v2/search.get", "v3/search.get", json!({
        "session":  SESSION,
        "query":    p.q,
        "duration": p.duration,
        "limit":    cfg.max_rows as u64,
    })).await {
        Ok(v)  => v,
        Err(e) => return Ok(Html(LogAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows:   0,
            q:        p.q.clone(),
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    format!("Could not fetch logs for analysis: {e}"),
        }.render()?)),
    };

    // Pull just the hit objects — `v4/llm.analyze kind=supplied` takes
    // a flat `rows` array and runs json_fingerprint over each entry,
    // so the structure can stay whatever the search returned.
    let rows: Vec<serde_json::Value> = search.get("results")
        .and_then(|r| r.as_array()).cloned().unwrap_or_default();
    let n_rows = rows.len();

    if n_rows == 0 {
        return Ok(Html(LogAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows:   0,
            q:        p.q.clone(),
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    format!(
                "Search for `{}` over the last {} returned no rows — nothing for the LLM to analyze.",
                p.q, p.duration
            ),
        }.render()?));
    }

    // Hand the rows to v4/llm.analyze.  `v4/*` requires an HMAC
    // signature on every call, so we go through `signed_rpc` rather
    // than the unauthenticated `rpc` — matches the `routes::chat`
    // pattern for `v4/llm.chat`.  No `provider` / `model` / `options`
    // keys → uses the bdsnode default provider with its default model
    // and num_ctx auto-bumps inside the analyze helper.
    // `kind: "supplied"` keeps the rows verbatim — the helper doesn't
    // re-route through aggregation_search.
    //
    // The per-call timeout and the prompt template are both read from
    // `web.analyze.logs.*` in bds.hjson (see state::LogsAnalyzeConfig).
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
            Ok(Html(LogAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                n_rows,
                q:        p.q,
                duration: p.duration,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(LogAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows,
            q:        p.q,
            duration: p.duration,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
