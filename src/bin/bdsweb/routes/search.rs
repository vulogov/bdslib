use askama::Template;
use axum::{extract::{Form, Query, State}, response::Html};
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
pub struct ObsHit {
    pub timestamp: String,
    pub key:       String,
    pub data:      String,
    pub score:     String,
}

#[derive(Debug)]
pub struct DocHit {
    pub name:     String,
    pub category: String,
    pub score:    String,
    pub preview:  String,
}

fn to_obs(arr: &serde_json::Value) -> Vec<ObsHit> {
    arr.as_array().map(|a| a.iter().map(|v| {
        let ts = v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
        let data = v.get("data").map(|d| d.to_string()).unwrap_or_default();
        ObsHit {
            timestamp: fmt_ts(ts),
            key:       v.get("key").and_then(|x| x.as_str()).unwrap_or("—").to_owned(),
            data:      truncate(&data, 100),
            score:     v.get("_score").and_then(|x| x.as_f64())
                        .map(|f| format!("{f:.3}")).unwrap_or_else(|| "—".to_owned()),
        }
    }).collect()).unwrap_or_default()
}

fn to_docs(arr: &serde_json::Value) -> Vec<DocHit> {
    arr.as_array().map(|a| a.iter().map(|v| {
        let meta = v.get("metadata").cloned().unwrap_or_default();
        let name = meta.get("name").or_else(|| meta.get("document_name"))
                       .and_then(|x| x.as_str()).unwrap_or("Untitled").to_owned();
        let content = v.get("document").and_then(|x| x.as_str()).unwrap_or("");
        DocHit {
            name,
            category: meta.get("category").and_then(|x| x.as_str()).unwrap_or("—").to_owned(),
            score:    v.get("score").and_then(|x| x.as_f64())
                       .map(|f| format!("{f:.3}")).unwrap_or_else(|| "—".to_owned()),
            preview:  truncate(content, 200),
        }
    }).collect()).unwrap_or_default()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_owned() } else { format!("{}…", &s[..n]) }
}

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "search.html")]
struct SearchPage {
    duration:         String,
    q:                String,
    mode_badge:       ModeBadge,
    /// Default LLM provider id, surfaced on a `data-` attribute so
    /// `onAnalyzeClick()` can name the actual upstream in the wait
    /// message.  Empty when bdsnode reports no providers registered.
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
    Ok(Html(SearchPage {
        duration: p.duration,
        q:        p.q,
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/search_panels.html")]
struct SearchPanels {
    obs:      Vec<ObsHit>,
    docs:     Vec<DocHit>,
    q:        String,
    duration: String,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    if p.q.is_empty() {
        return Ok(Html(SearchPanels { obs: vec![], docs: vec![], q: p.q, duration: p.duration }.render()?));
    }

    let resp = rpc_versioned(&state, "v2/aggregationsearch", "v3/aggregationsearch", json!({
        "session":  SESSION,
        "query":    p.q,
        "duration": p.duration,
    })).await?;

    Ok(Html(SearchPanels {
        obs:      to_obs(&resp["observability"]),
        docs:     to_docs(&resp["documents"]),
        q:        p.q,
        duration: p.duration,
    }.render()?))
}

// ── HTMX: "Analyze this!" — one-shot LLM over telemetry + docs ───────────────
//
// The aggregated-search page is unique in that its result set is two
// correlated corpora — `observability` (telemetry rows) and
// `documents` (matched op-docs).  The whole point of the page is for
// the operator to read them together, so the LLM analysis hand-off
// has to preserve that distinction.  Each row gets a synthetic
// `_kind` field (`"telemetry"` or `"document"`) before being handed
// to `v4/llm.analyze` with `kind=supplied` — `json_fingerprint`
// flattens that into the prompt, so the model can tell which corpus
// each evidence row came from.

#[derive(Template)]
#[template(path = "partials/agg_search_analysis.html")]
struct AggSearchAnalysis {
    response:      String,
    response_html: String,
    provider:      String,
    model:         String,
    ms:            u64,
    /// Total rows handed to the LLM (telemetry + documents).
    n_rows:        usize,
    /// Per-corpus counts ACTUALLY fed to the LLM after the
    /// `max_rows` budget split.  May be less than the matched
    /// counts when the result set is larger than the budget.
    n_telemetry:   usize,
    n_documents:   usize,
    /// What `v?/aggregationsearch` matched before the budget split —
    /// surfaced in the strip as "of N matched" so the operator can
    /// tell when the prompt is sampling rather than seeing the
    /// whole result set.
    matched_telemetry: usize,
    matched_documents: usize,
    q:             String,
    duration:      String,
    /// `"miss"`, `"hit"`, or `""`.
    cache:         String,
    /// Empty when the LLM ran cleanly; banner message otherwise.
    error:         String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Form(p): Form<Params>,
) -> Result<Html<String>, AppError> {
    if p.q.is_empty() {
        return Ok(Html(AggSearchAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows:   0,
            n_telemetry: 0,
            n_documents: 0,
            matched_telemetry: 0,
            matched_documents: 0,
            q:        p.q.clone(),
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    "Run a search first — there's nothing for the LLM to correlate yet.".to_owned(),
        }.render()?));
    }

    let cfg = state.agg_search_analyze.clone();

    // Re-run the same aggregationsearch the page just executed so the
    // LLM sees exactly the operator's view.  Errors render in the
    // panel rather than 500-ing.
    let resp = match rpc_versioned(&state, "v2/aggregationsearch", "v3/aggregationsearch", json!({
        "session":  SESSION,
        "query":    p.q,
        "duration": p.duration,
    })).await {
        Ok(v)  => v,
        Err(e) => return Ok(Html(AggSearchAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows:   0,
            n_telemetry: 0,
            n_documents: 0,
            matched_telemetry: 0,
            matched_documents: 0,
            q:        p.q.clone(),
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    format!("Could not fetch aggregated search results for analysis: {e}"),
        }.render()?)),
    };

    let telemetry: Vec<serde_json::Value> = resp.get("observability")
        .and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let documents: Vec<serde_json::Value> = resp.get("documents")
        .and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let matched_telemetry = telemetry.len();
    let matched_documents = documents.len();

    if matched_telemetry == 0 && matched_documents == 0 {
        return Ok(Html(AggSearchAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows:   0,
            n_telemetry: 0,
            n_documents: 0,
            matched_telemetry: 0,
            matched_documents: 0,
            q:        p.q.clone(),
            duration: p.duration.clone(),
            cache:    String::new(),
            error:    format!(
                "Search for `{}` over the last {} returned no telemetry and no documents — \
                 nothing for the LLM to analyze.",
                p.q, p.duration
            ),
        }.render()?));
    }

    // Split `cfg.max_rows` between the two corpora so a query that
    // matches lots of telemetry can't crowd out all the documents —
    // which is what was happening when both arrays were concatenated
    // and then truncated telemetry-first.  Reserve up to half the
    // budget for documents (they cap at ~10 in v?/aggregationsearch
    // and each carries far more analytical value per row); the rest
    // goes to telemetry.  When one corpus is empty, the other gets
    // the full budget.
    let doc_budget = (cfg.max_rows / 2).max(1);
    let n_documents = matched_documents.min(doc_budget);
    let n_telemetry = matched_telemetry.min(cfg.max_rows.saturating_sub(n_documents));

    // Tag each row with `_kind` so the supplied-payload fingerprint
    // tells the LLM which corpus the evidence is from.  Documents
    // are usually large blobs — keep just `metadata` + a clipped
    // `document` preview so the prompt budget doesn't blow up.
    let mut rows: Vec<serde_json::Value> = Vec::with_capacity(n_telemetry + n_documents);
    for t in telemetry.iter().take(n_telemetry) {
        let mut obj = t.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("_kind".into(), json!("telemetry"));
        }
        rows.push(obj);
    }
    for d in documents.iter().take(n_documents) {
        let name = d.get("metadata").and_then(|m| m.get("name"))
            .and_then(|x| x.as_str()).unwrap_or("Untitled").to_owned();
        let category = d.get("metadata").and_then(|m| m.get("category"))
            .and_then(|x| x.as_str()).unwrap_or("").to_owned();
        let preview = d.get("document").and_then(|x| x.as_str()).unwrap_or("");
        // 800 chars per doc preview — gives the model a usable
        // excerpt without letting one 100kB runbook crowd out the
        // telemetry rows.
        let clipped = if preview.len() > 800 {
            format!("{}…", &preview[..800])
        } else {
            preview.to_owned()
        };
        rows.push(json!({
            "_kind":   "document",
            "name":    name,
            "category": category,
            "preview": clipped,
        }));
    }
    let n_rows = rows.len();

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
            Ok(Html(AggSearchAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                n_rows, n_telemetry, n_documents,
                matched_telemetry, matched_documents,
                q:        p.q,
                duration: p.duration,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(AggSearchAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_rows, n_telemetry, n_documents,
            matched_telemetry, matched_documents,
            q:        p.q,
            duration: p.duration,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
