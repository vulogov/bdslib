use askama::Template;
use axum::{extract::{Query, State}, response::Html};
use serde::Deserialize;
use serde_json::json;

use crate::{admin::signed_rpc, admin::signed_rpc_with_timeout, client::{fmt_ts, mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};
use super::rca::{extract_rca, CausalRow, ClusterCard, RcaSummary};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default)]
    pub failure_body: String,
    #[serde(default = "default_bucket_secs")]
    pub bucket_secs: u64,
    #[serde(default = "default_min_support")]
    pub min_support: usize,
    #[serde(default = "default_jaccard")]
    pub jaccard_threshold: f64,
    #[serde(default = "default_max_keys")]
    pub max_keys: usize,
}
fn default_duration()    -> String { "1h".to_owned() }
fn default_bucket_secs() -> u64    { 300 }
fn default_min_support() -> usize  { 2 }
fn default_jaccard()     -> f64    { 0.2 }
fn default_max_keys()    -> usize  { 200 }

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "rca_templates.html")]
struct RcaTemplatesPage {
    duration:          String,
    failure_body:      String,
    bucket_secs:       u64,
    min_support:       usize,
    jaccard_threshold: f64,
    max_keys:          usize,
    mode_badge:        ModeBadge,
    /// Default LLM provider id, surfaced on `data-` for the
    /// wait-message JS.  Empty when bdsnode reports no providers.
    analyze_provider:  String,
    /// Default model name.  Empty when unavailable.
    analyze_model:     String,
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
            log::warn!("[rca_templates] v4/llm.providers.list failed: {e}");
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
    Ok(Html(RcaTemplatesPage {
        duration:          p.duration,
        failure_body:      p.failure_body,
        bucket_secs:       p.bucket_secs,
        min_support:       p.min_support,
        jaccard_threshold: p.jaccard_threshold,
        max_keys:          p.max_keys,
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/rca_templates_results.html")]
struct RcaTemplatesResults {
    duration:   String,
    summary:    RcaSummary,
    causes:     Vec<CausalRow>,
    clusters:   Vec<ClusterCard>,
    mode_badge: ModeBadge,
}

/// Rename the few field names that differ between v2/rca and v2/rca.templates
/// so that the shared `extract_rca` extractor can consume the response without
/// duplication (v2/rca uses `failure_key` and `probable_causes[].key`, whereas
/// v2/rca.templates uses `failure_body` and `probable_causes[].body`).
fn normalize_to_rca_shape(mut v: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = v.as_object_mut() {
        if let Some(fb) = obj.remove("failure_body") {
            obj.insert("failure_key".to_owned(), fb);
        }
        if let Some(causes) = obj.get_mut("probable_causes").and_then(|x| x.as_array_mut()) {
            for c in causes {
                if let Some(c_obj) = c.as_object_mut() {
                    if let Some(body) = c_obj.remove("body") {
                        c_obj.insert("key".to_owned(), body);
                    }
                }
            }
        }
    }
    v
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let fb: Option<String> = if p.failure_body.is_empty() { None } else { Some(p.failure_body.clone()) };

    let resp = rpc_versioned(&state, "v2/rca.templates", "v3/rca.templates", json!({
        "session":           SESSION,
        "duration":          p.duration,
        "failure_body":      fb,
        "bucket_secs":       p.bucket_secs,
        "min_support":       p.min_support,
        "jaccard_threshold": p.jaccard_threshold,
        "max_keys":          p.max_keys,
    })).await?;

    let mode_badge = ModeBadge::from_response(&resp);
    let (summary, causes, clusters) = extract_rca(&normalize_to_rca_shape(resp));

    Ok(Html(RcaTemplatesResults { duration: p.duration, summary, causes, clusters, mode_badge }.render()?))
}

// ── HTMX: "Analyze this!" — template-level in-depth RCA ──────────────────────
//
// Re-runs `v?/rca.templates` with the same params and ships a
// 2-output payload to v4/llm.analyze.  Unlike the inline `results`
// path, the LLM payload preserves the original field names from
// `v?/rca.templates` (`failure_body`, `body`) rather than running
// them through `normalize_to_rca_shape` — the prompt is tuned for
// template-level vocabulary and renaming `body → key` would
// misalign it.

/// Same 60/40 budget split as `routes::rca` / `denoise` / `knn`.
fn split_two_corpus_budget(
    matched_a: usize,
    matched_b: usize,
    budget:    usize,
) -> (usize, usize) {
    if budget == 0 { return (0, 0); }
    if matched_a + matched_b <= budget {
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
#[template(path = "partials/rca_templates_analysis.html")]
struct RcaTemplatesAnalysis {
    response:          String,
    response_html:     String,
    provider:          String,
    model:             String,
    ms:                u64,
    /// Resolved failure template body (auto-picked when the
    /// operator left `failure_body` empty).  Echoed in the panel
    /// header so the operator sees exactly what was investigated.
    failure_body:      String,
    /// Cause rows fed to the LLM (after the budget split).
    n_causes_fed:      usize,
    /// Cluster rows fed to the LLM (after the budget split).
    n_clusters_fed:    usize,
    /// Totals returned by v?/rca.templates before the budget split.
    matched_causes:    usize,
    matched_clusters:  usize,
    n_events:          u64,
    n_keys:            u64,
    duration:          String,
    /// `"miss"`, `"hit"`, or `""`.
    cache:             String,
    /// Empty when the LLM ran cleanly; banner message otherwise.
    error:             String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let cfg = state.rca_templates_analyze.clone();
    let fb: Option<String> = if p.failure_body.is_empty() { None } else { Some(p.failure_body.clone()) };

    let resp = match rpc_versioned(&state, "v2/rca.templates", "v3/rca.templates", json!({
        "session":           SESSION,
        "duration":          p.duration.clone(),
        "failure_body":      fb,
        "bucket_secs":       p.bucket_secs,
        "min_support":       p.min_support,
        "jaccard_threshold": p.jaccard_threshold,
        "max_keys":          p.max_keys,
    })).await {
        Ok(v)  => v,
        Err(e) => return Ok(Html(RcaTemplatesAnalysis {
            response:          String::new(),
            response_html:     String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:                0,
            failure_body:      p.failure_body.clone(),
            n_causes_fed:      0,
            n_clusters_fed:    0,
            matched_causes:    0,
            matched_clusters:  0,
            n_events:          0,
            n_keys:             0,
            duration:          p.duration.clone(),
            cache:    String::new(),
            error:    format!("Could not fetch templates-RCA results for analysis: {e}"),
        }.render()?)),
    };

    // Pull raw fields — keep the original v?/rca.templates naming
    // (`failure_body`, `body`) for the LLM payload so the prompt's
    // template-level vocabulary lines up.
    let resolved_failure_body = resp.get("failure_body")
        .and_then(|x| x.as_str()).unwrap_or("").to_owned();
    let start    = resp.get("start").and_then(|x| x.as_u64()).unwrap_or(0);
    let end      = resp.get("end").and_then(|x| x.as_u64()).unwrap_or(0);
    let n_events = resp.get("n_events").and_then(|x| x.as_u64()).unwrap_or(0);
    let n_keys   = resp.get("n_keys").and_then(|x| x.as_u64()).unwrap_or(0);

    let causes_raw: Vec<serde_json::Value> = resp.get("probable_causes")
        .and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let clusters_raw: Vec<serde_json::Value> = resp.get("clusters")
        .and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let matched_causes   = causes_raw.len();
    let matched_clusters = clusters_raw.len();

    if matched_causes == 0 && matched_clusters == 0 {
        return Ok(Html(RcaTemplatesAnalysis {
            response:          String::new(),
            response_html:     String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:                0,
            failure_body:      resolved_failure_body.clone(),
            n_causes_fed:      0,
            n_clusters_fed:    0,
            matched_causes:    0,
            matched_clusters:  0,
            n_events,
            n_keys,
            duration:          p.duration.clone(),
            cache:    String::new(),
            error:    format!(
                "Templates RCA found no probable cause templates and no clusters in the \
                 last {} ({} event{}, {} template{} scanned) — try lowering \
                 `min_support` or `jaccard_threshold`, widen the duration, or check \
                 that drain3 has mined enough templates yet.",
                p.duration, n_events, if n_events == 1 { "" } else { "s" },
                n_keys, if n_keys == 1 { "" } else { "s" }
            ),
        }.render()?));
    }

    // 60/40 split favouring causes.
    let (n_causes_fed, n_clusters_fed) = split_two_corpus_budget(
        matched_causes, matched_clusters, cfg.max_rows,
    );

    let mut rows: Vec<serde_json::Value> = Vec::with_capacity(n_causes_fed + n_clusters_fed + 1);

    // Stats row anchors the failure template identification.
    rows.push(json!({
        "_kind":             "rca_templates_window_stats",
        "failure_body":      resolved_failure_body,
        "failure_body_was_auto_picked": p.failure_body.is_empty(),
        "window_start":      start,
        "window_end":        end,
        "window_start_iso":  fmt_ts(start),
        "window_end_iso":    fmt_ts(end),
        "n_events":          n_events,
        "n_keys":            n_keys,
        "bucket_secs":       p.bucket_secs,
        "min_support":       p.min_support,
        "jaccard_threshold": p.jaccard_threshold,
        "max_keys":          p.max_keys,
        "duration":          p.duration,
        "n_causes_total":    matched_causes,
        "n_clusters_total":  matched_clusters,
        "n_causes_fed":      n_causes_fed,
        "n_clusters_fed":    n_clusters_fed,
    }));

    // Cause rows — add _kind, rank, and is_precursor (synthesised
    // from avg_lead_secs sign so the model doesn't recompute it).
    // Keep the original `body` field name from v?/rca.templates so
    // the prompt's template-level vocabulary stays aligned.
    for (i, c) in causes_raw.iter().take(n_causes_fed).enumerate() {
        let lead_secs = c.get("avg_lead_secs").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let mut obj = c.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("_kind".into(),        json!("rca_templates_cause"));
            m.insert("rank".into(),         json!(i + 1));
            m.insert("is_precursor".into(), json!(lead_secs >= 0.0));
        }
        rows.push(obj);
    }

    // Cluster rows — tag in place.  Cluster members are template
    // bodies (with `<*>` wildcards) coming from the detector
    // directly; no further projection needed.
    for c in clusters_raw.iter().take(n_clusters_fed) {
        let mut obj = c.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("_kind".into(), json!("rca_templates_cluster"));
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
            Ok(Html(RcaTemplatesAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                failure_body:  resolved_failure_body,
                n_causes_fed, n_clusters_fed,
                matched_causes, matched_clusters,
                n_events, n_keys,
                duration: p.duration,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(RcaTemplatesAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            failure_body:  resolved_failure_body,
            n_causes_fed, n_clusters_fed,
            matched_causes, matched_clusters,
            n_events, n_keys,
            duration: p.duration,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
