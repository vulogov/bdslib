use askama::Template;
use axum::{extract::{Form, Query, State}, response::Html};
use serde::Deserialize;
use serde_json::json;

use crate::{admin::signed_rpc_with_timeout, client::{fmt_ts, mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default)]
    pub q: String,
}
fn default_duration() -> String { "1h".to_owned() }

// ── Row data type ─────────────────────────────────────────────────────────────

pub struct TplRow {
    #[allow(dead_code)]
    pub id:        String,
    pub name:      String,
    pub body:      String,
    pub timestamp: String,
    pub score:     String,
}

fn row_from_recent(v: &serde_json::Value) -> TplRow {
    let id   = v.get("id").and_then(|x| x.as_str()).unwrap_or("—").to_owned();
    let meta = v.get("metadata").cloned().unwrap_or_default();
    let name = meta.get("name").and_then(|x| x.as_str()).unwrap_or("—").to_owned();
    let ts   = meta.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
    let body = v.get("body").and_then(|x| x.as_str()).unwrap_or("—").to_owned();
    TplRow { id, name, body, timestamp: fmt_ts(ts), score: String::new() }
}

fn row_from_search(v: &serde_json::Value) -> TplRow {
    let id   = v.get("id").and_then(|x| x.as_str()).unwrap_or("—").to_owned();
    let meta = v.get("metadata").cloned().unwrap_or_default();
    let name = meta.get("name").and_then(|x| x.as_str()).unwrap_or("—").to_owned();
    let ts   = meta.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
    let body = v.get("document").and_then(|x| x.as_str()).unwrap_or("—").to_owned();
    let score = v.get("score").and_then(|x| x.as_f64())
                  .map(|f| format!("{f:.3}"))
                  .unwrap_or_else(|| "—".to_owned());
    TplRow { id, name, body, timestamp: fmt_ts(ts), score }
}

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "templates.html")]
struct TemplatesPage {
    duration:         String,
    q:                String,
    mode_badge:       ModeBadge,
    /// Default LLM provider id for "Analyze this!" — surfaced on a
    /// `data-` attribute so the wait-message JS can name the actual
    /// upstream.  Empty when bdsnode reports no providers registered.
    analyze_provider: String,
    /// Default model name for that provider.  Empty when unavailable.
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
    Ok(Html(TemplatesPage {
        duration: p.duration,
        q:        p.q,
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/template_rows.html")]
struct TemplateRows {
    rows:       Vec<TplRow>,
    duration:   String,
    q:          String,
    searching:  bool,
    mode_badge: ModeBadge,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    if p.q.is_empty() {
        // Browse mode: show recently observed templates via FrequencyTracking
        let resp = rpc_versioned(&state, "v2/tpl.templates_recent", "v3/tpl.templates_recent", json!({
            "session":  SESSION,
            "duration": p.duration,
        }))
        .await
        .unwrap_or_default();

        let mode_badge = ModeBadge::from_response(&resp);
        let rows = resp
            .get("templates")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().map(row_from_recent).collect())
            .unwrap_or_default();

        Ok(Html(TemplateRows { rows, duration: p.duration, q: p.q, searching: false, mode_badge }.render()?))
    } else {
        // Search mode: semantic vector search across tpl store
        let resp = rpc_versioned(&state, "v2/tpl.search", "v3/tpl.search", json!({
            "session":  SESSION,
            "duration": p.duration,
            "query":    p.q,
            "limit":    50,
        }))
        .await
        .unwrap_or_default();

        let mode_badge = ModeBadge::from_response(&resp);
        let rows = resp
            .get("results")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().map(row_from_search).collect())
            .unwrap_or_default();

        Ok(Html(TemplateRows { rows, duration: p.duration, q: p.q, searching: true, mode_badge }.render()?))
    }
}

// ── HTMX: "Analyze this!" — one-shot LLM analysis of drain3 templates ────────
//
// Mirror of `routes::logs::analyze` / `routes::telemetry::analyze`,
// but the underlying RPCs are `v?/tpl.templates_recent` (browse mode,
// empty `q`) and `v?/tpl.search` (semantic search, non-empty `q`).
// Reads its prompt + timeout from `state.templates_analyze`.

#[derive(Template)]
#[template(path = "partials/templates_analysis.html")]
struct TemplatesAnalysis {
    response:      String,
    response_html: String,
    provider:      String,
    model:         String,
    ms:            u64,
    n_rows:        usize,
    q:             String,
    duration:      String,
    /// `"miss"`, `"hit"`, or `""`.
    cache:         String,
    /// Empty when the LLM ran cleanly; a short banner message when
    /// the route caught a v4/v2 failure but still wants to render
    /// the panel.
    error:         String,
    /// `true` when the operator had a non-empty query in the search
    /// box (semantic search mode); `false` for the browse-recent
    /// path.  Echoed back so the panel header tells them which set
    /// of templates was analysed.
    searched:      bool,
}

pub async fn analyze(
    State(state): State<AppState>,
    Form(p): Form<Params>,
) -> Result<Html<String>, AppError> {
    let cfg = state.templates_analyze.clone();
    let searched = !p.q.is_empty();

    // Pull the same templates the page is showing — browse-recent when
    // q is empty, semantic search when set.  Errors render in the
    // panel rather than 500-ing.
    let resp = if searched {
        rpc_versioned(&state, "v2/tpl.search", "v3/tpl.search", json!({
            "session":  SESSION,
            "duration": p.duration,
            "query":    p.q,
            "limit":    cfg.max_rows as u64,
        })).await
    } else {
        rpc_versioned(&state, "v2/tpl.templates_recent", "v3/tpl.templates_recent", json!({
            "session":  SESSION,
            "duration": p.duration,
        })).await
    };

    let resp = match resp {
        Ok(v)  => v,
        Err(e) => return Ok(Html(TemplatesAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows:   0,
            q:        p.q.clone(),
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    format!("Could not fetch templates for analysis: {e}"),
            searched,
        }.render()?)),
    };

    // Both RPCs report rows under a different field name.  Pull just
    // the relevant subset (id, name, body, timestamp) so the LLM sees
    // the meaningful surface area and we don't blow the prompt
    // budget on internal HNSW scoring metadata.
    let raw_rows: Vec<serde_json::Value> = if searched {
        resp.get("results").and_then(|x| x.as_array()).cloned().unwrap_or_default()
    } else {
        resp.get("templates").and_then(|x| x.as_array()).cloned().unwrap_or_default()
    };

    // For the recent path the template body lives at `body`; for the
    // search path it's at `document`.  Project both shapes into a
    // single `{id, name, ts, body}` form so the LLM sees consistent
    // structure regardless of which call produced the rows.
    let take_n = cfg.max_rows;
    let projected: Vec<serde_json::Value> = raw_rows.iter().take(take_n).map(|r| {
        let id   = r.get("id").and_then(|x| x.as_str()).unwrap_or("").to_owned();
        let meta = r.get("metadata").cloned().unwrap_or_default();
        let name = meta.get("name").and_then(|x| x.as_str()).unwrap_or("").to_owned();
        let ts   = meta.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
        let body = r.get("body").and_then(|x| x.as_str())
            .or_else(|| r.get("document").and_then(|x| x.as_str()))
            .unwrap_or("").to_owned();
        json!({ "id": id, "name": name, "ts": ts, "body": body })
    }).collect();
    let n_rows = projected.len();

    if n_rows == 0 {
        let banner = if searched {
            format!(
                "Search for `{}` over the last {} returned no templates — nothing for the LLM to analyze.",
                p.q, p.duration
            )
        } else {
            format!(
                "No templates observed in the last {} — nothing for the LLM to analyze.  Wait for drain3 \
                 to mine some patterns or widen the duration window.",
                p.duration
            )
        };
        return Ok(Html(TemplatesAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows:   0,
            q:        p.q.clone(),
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    banner,
            searched,
        }.render()?));
    }

    // Hand the projected rows to v4/llm.analyze (HMAC-signed).  Prompt
    // and timeout come from `web.analyze.templates.*`.
    let analyze_resp = signed_rpc_with_timeout(
        &state,
        "v4/llm.analyze",
        json!({
            "kind":            "supplied",
            "rows":            projected,
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
            Ok(Html(TemplatesAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                n_rows,
                q:        p.q,
                duration: p.duration,
                cache,
                error:    String::new(),
                searched,
            }.render()?))
        }
        Err(e) => Ok(Html(TemplatesAnalysis {
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
            searched,
        }.render()?)),
    }
}
