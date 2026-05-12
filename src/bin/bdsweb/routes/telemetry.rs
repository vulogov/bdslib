use askama::Template;
use axum::{
    extract::{Query, State},
    response::Html,
};
use serde::Deserialize;
use serde_json::json;

use crate::{admin::signed_rpc, admin::signed_rpc_with_timeout, client::{fmt_ts, mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default)]
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}
fn default_duration() -> String { "1h".to_owned() }
fn default_limit()    -> usize  { 50 }

/// Clamp a user-supplied limit to the supported [1, 1000] range.
fn clamp_limit(n: usize) -> usize { n.clamp(1, 1000) }

#[derive(Debug)]
pub struct HitRow {
    pub timestamp: String,
    pub key:       String,
    pub data:      String,
    pub score:     String,
}

fn to_rows(results: &serde_json::Value) -> Vec<HitRow> {
    results.as_array()
           .map(|arr| arr.iter().map(hit_to_row).collect())
           .unwrap_or_default()
}

fn hit_to_row(v: &serde_json::Value) -> HitRow {
    let ts   = v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
    let data = v.get("data").map(|d| d.to_string()).unwrap_or_default();
    let score = v.get("_score").and_then(|x| x.as_f64())
                 .map(|f| format!("{f:.3}"))
                 .unwrap_or_else(|| "—".to_owned());
    HitRow {
        timestamp: fmt_ts(ts),
        key:       v.get("key").and_then(|x| x.as_str()).unwrap_or("—").to_owned(),
        data,
        score,
    }
}

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "telemetry.html")]
struct TelemetryPage {
    duration:         String,
    q:                String,
    limit:            usize,
    mode_badge:       ModeBadge,
    /// Default LLM provider id for "Analyze this!" (`"ollama"`,
    /// `"deepseek"`, …).  Empty when bdsnode reports no providers
    /// registered.  Surfaced on a `data-` attribute so the wait
    /// message JS can name the actual provider.
    analyze_provider: String,
    /// Default model name for that provider.  Empty when unavailable.
    analyze_model:    String,
}

/// Fetch the default v4/llm provider + model so the wait-message JS
/// can name the actual upstream.  All failure modes (missing HMAC
/// secret, bdsnode unreachable, empty list, etc.) collapse to
/// `("", "")` and the page falls back to a generic phrasing.
async fn fetch_analyze_provider(state: &AppState) -> (String, String) {
    if state.shared_secret.is_empty() {
        return (String::new(), String::new());
    }
    let resp = match signed_rpc(state, "v4/llm.providers.list", json!({})).await {
        Ok(v)  => v,
        Err(e) => {
            log::warn!("[telemetry] v4/llm.providers.list failed: {e}");
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
    Ok(Html(TelemetryPage {
        duration: p.duration,
        q:        p.q,
        limit:    clamp_limit(p.limit),
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
        href_base: "/telemetry".to_owned(),
    }.render()?))
}

// ── HTMX: vector search results fragment ─────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/telemetry_rows.html")]
struct TelemetryRows {
    rows:     Vec<HitRow>,
    duration: String,
    q:        String,
    mode_badge: Option<ModeBadge>,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let limit = clamp_limit(p.limit);

    if p.q.is_empty() {
        return Ok(Html(TelemetryRows {
            rows: vec![], duration: p.duration, q: p.q, mode_badge: None,
        }.render()?));
    }

    let resp = rpc_versioned(&state, "v2/search.get", "v3/search.get", json!({
        "session":  SESSION,
        "query":    p.q,
        "duration": p.duration,
        "limit":    limit,
    })).await?;

    let mode_badge = Some(ModeBadge::from_response(&resp));

    Ok(Html(TelemetryRows {
        rows:     to_rows(&resp["results"]),
        duration: p.duration,
        q:        p.q,
        mode_badge,
    }.render()?))
}

// ── HTMX: "Analyze this!" — one-shot LLM analysis of metric rows ─────────────
//
// Mirror of `routes::logs::analyze`, but reads from
// `state.metrics_analyze` (configured via `web.analyze.metrics.*`) and
// uses a metric-focused default prompt.  Same response shape so the
// floating panel template (`partials/metrics_analysis.html`) maps 1:1
// onto its logs sibling.

#[derive(Template)]
#[template(path = "partials/metrics_analysis.html")]
struct MetricsAnalysis {
    /// Raw markdown body — kept around for the empty-check.  The
    /// template renders `response_html` (server-side sanitised HTML
    /// conversion) instead.
    response:      String,
    response_html: String,
    provider:      String,
    model:         String,
    ms:            u64,
    n_rows:        usize,
    q:             String,
    duration:      String,
    /// `"miss"`, `"hit"`, or `""` (when the cache layer didn't run).
    cache:         String,
    /// Empty when the LLM ran cleanly; a short banner message when
    /// the route caught a v4/v2 failure but still wants to render
    /// the panel.
    error:         String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    // No query → nothing to analyze; render a friendly hint.
    if p.q.is_empty() {
        return Ok(Html(MetricsAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows:   0,
            q:        p.q.clone(),
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    "Run a search first — there are no metrics to analyze yet.".to_owned(),
        }.render()?));
    }

    let cfg = state.metrics_analyze.clone();

    // Re-run the same search the page just executed so the LLM sees
    // exactly what the operator sees.  Errors here render in the
    // panel rather than 500-ing.  Row limit is operator-configurable
    // via `web.analyze.metrics.max_rows`.
    let search = match rpc_versioned(&state, "v2/search.get", "v3/search.get", json!({
        "session":  SESSION,
        "query":    p.q,
        "duration": p.duration,
        "limit":    cfg.max_rows as u64,
    })).await {
        Ok(v)  => v,
        Err(e) => return Ok(Html(MetricsAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows:   0,
            q:        p.q.clone(),
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    format!("Could not fetch metrics for analysis: {e}"),
        }.render()?)),
    };

    let rows: Vec<serde_json::Value> = search.get("results")
        .and_then(|r| r.as_array()).cloned().unwrap_or_default();
    let n_rows = rows.len();

    if n_rows == 0 {
        return Ok(Html(MetricsAnalysis {
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

    // Hand the rows to v4/llm.analyze (HMAC-signed).  No
    // provider/model/options overrides → bdsnode's default upstream
    // runs the inference.  The per-call timeout and prompt template
    // are read from `web.analyze.metrics.*` in bds.hjson.
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
            Ok(Html(MetricsAnalysis {
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
        Err(e) => Ok(Html(MetricsAnalysis {
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
