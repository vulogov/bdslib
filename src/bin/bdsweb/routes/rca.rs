use askama::Template;
use axum::{extract::{Query, State}, response::Html};
use serde::Deserialize;
use serde_json::json;

use crate::{admin::signed_rpc, admin::signed_rpc_with_timeout, client::{fmt_ts, mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default)]
    pub failure_key: String,
    #[serde(default = "default_bucket_secs")]
    pub bucket_secs: u64,
    #[serde(default = "default_min_support")]
    pub min_support: usize,
    #[serde(default = "default_jaccard")]
    pub jaccard_threshold: f64,
}
fn default_duration()    -> String { "1h".to_owned() }
fn default_bucket_secs() -> u64   { 300 }
fn default_min_support() -> usize  { 2 }
fn default_jaccard()     -> f64   { 0.2 }

// ── Template data types ───────────────────────────────────────────────────────

pub struct RcaSummary {
    pub failure_key:   String,
    pub has_failure:   bool,
    pub window_start:  String,
    pub window_end:    String,
    pub n_events:      usize,
    pub n_keys:        usize,
    pub cluster_count: usize,
    pub cause_count:   usize,
}

pub struct CausalRow {
    pub rank:         usize,
    pub key:          String,
    pub co_count:     usize,
    pub jaccard:      String,
    pub lead_label:   String,
    pub lead_bar_pct: u8,
    pub is_precursor: bool,
    pub lead_cls:     String,
    pub bar_cls:      String,
}

pub struct ClusterCard {
    pub id:                usize,
    pub members:           Vec<String>,
    pub support:           usize,
    pub cohesion:          String,
    pub cohesion_pct:      u8,
    pub cohesion_bar_cls:  String,
    pub cohesion_badge_cls: String,
}

// ── Data extraction ───────────────────────────────────────────────────────────

fn fmt_lead_label(secs: f64) -> String {
    let abs = secs.abs();
    if abs < 1.0 { return "simultaneous".to_owned(); }
    let mins = (abs / 60.0) as u64;
    let s    = (abs % 60.0) as u64;
    let t = if mins > 0 { format!("{mins}m {s:02}s") } else { format!("{s}s") };
    if secs > 0.0 { format!("{t} before") } else { format!("{t} after") }
}

pub(super) fn extract_rca(v: &serde_json::Value) -> (RcaSummary, Vec<CausalRow>, Vec<ClusterCard>) {
    let failure_key = v.get("failure_key")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_owned();
    let has_failure = !failure_key.is_empty();

    let start    = v.get("start").and_then(|x| x.as_u64()).unwrap_or(0);
    let end      = v.get("end").and_then(|x| x.as_u64()).unwrap_or(0);
    let n_events = v.get("n_events").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    let n_keys   = v.get("n_keys").and_then(|x| x.as_u64()).unwrap_or(0) as usize;

    // ── Clusters ──────────────────────────────────────────────────────────────
    let clusters: Vec<ClusterCard> = v
        .get("clusters")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter().map(|c| {
                let id         = c.get("id").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                let support    = c.get("support").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                let cohesion_f = c.get("cohesion").and_then(|x| x.as_f64()).unwrap_or(0.0);
                let cohesion_pct = (cohesion_f * 100.0).clamp(0.0, 100.0) as u8;
                let members: Vec<String> = c
                    .get("members")
                    .and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                let (cohesion_bar_cls, cohesion_badge_cls) = if cohesion_pct >= 70 {
                    ("bg-green-600", "bg-green-900 text-green-300")
                } else if cohesion_pct >= 40 {
                    ("bg-blue-600", "bg-blue-900 text-blue-300")
                } else {
                    ("bg-slate-600", "bg-slate-800 text-slate-400")
                };
                ClusterCard {
                    id,
                    members,
                    support,
                    cohesion: format!("{cohesion_f:.2}"),
                    cohesion_pct,
                    cohesion_bar_cls:  cohesion_bar_cls.to_owned(),
                    cohesion_badge_cls: cohesion_badge_cls.to_owned(),
                }
            }).collect()
        })
        .unwrap_or_default();

    // ── Probable causes ───────────────────────────────────────────────────────
    let causes_raw: Vec<serde_json::Value> = v
        .get("probable_causes")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();

    let max_abs_lead = causes_raw
        .iter()
        .filter_map(|c| c.get("avg_lead_secs").and_then(|x| x.as_f64()))
        .map(|f| f.abs())
        .fold(0.0f64, f64::max);

    let causes: Vec<CausalRow> = causes_raw.iter().enumerate().map(|(i, c)| {
        let key      = c.get("key").and_then(|x| x.as_str()).unwrap_or("—").to_owned();
        let co_count = c.get("co_occurrence_count").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
        let jaccard  = c.get("jaccard").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let lead     = c.get("avg_lead_secs").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let is_precursor = lead >= 0.0;
        let lead_bar_pct = if max_abs_lead > 0.0 {
            ((lead.abs() / max_abs_lead) * 100.0).clamp(0.0, 100.0) as u8
        } else { 0 };
        CausalRow {
            rank: i + 1,
            key,
            co_count,
            jaccard: format!("{jaccard:.2}"),
            lead_label: fmt_lead_label(lead),
            lead_bar_pct,
            is_precursor,
            lead_cls: if is_precursor { "text-green-400" } else { "text-amber-400" }.to_owned(),
            bar_cls:  if is_precursor { "bg-green-600"  } else { "bg-amber-600"  }.to_owned(),
        }
    }).collect();

    let summary = RcaSummary {
        failure_key,
        has_failure,
        window_start:  fmt_ts(start),
        window_end:    fmt_ts(end),
        n_events,
        n_keys,
        cluster_count: clusters.len(),
        cause_count:   causes.len(),
    };

    (summary, causes, clusters)
}

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "rca.html")]
struct RcaPage {
    duration:          String,
    failure_key:       String,
    bucket_secs:       u64,
    min_support:       usize,
    jaccard_threshold: f64,
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
            log::warn!("[rca] v4/llm.providers.list failed: {e}");
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
    Ok(Html(RcaPage {
        duration:          p.duration,
        failure_key:       p.failure_key,
        bucket_secs:       p.bucket_secs,
        min_support:       p.min_support,
        jaccard_threshold: p.jaccard_threshold,
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/rca_results.html")]
struct RcaResults {
    duration:   String,
    summary:    RcaSummary,
    causes:     Vec<CausalRow>,
    clusters:   Vec<ClusterCard>,
    mode_badge: ModeBadge,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let fk: Option<String> = if p.failure_key.is_empty() { None } else { Some(p.failure_key.clone()) };

    let resp = rpc_versioned(&state, "v2/rca", "v3/rca", json!({
        "session":           SESSION,
        "duration":          p.duration,
        "failure_key":       fk,
        "bucket_secs":       p.bucket_secs,
        "min_support":       p.min_support,
        "jaccard_threshold": p.jaccard_threshold,
    })).await?;

    let (summary, causes, clusters) = extract_rca(&resp);
    let mode_badge = ModeBadge::from_response(&resp);

    Ok(Html(RcaResults { duration: p.duration, summary, causes, clusters, mode_badge }.render()?))
}

// ── HTMX: "Analyze this!" — in-depth RCA reasoning via LLM ───────────────────
//
// Re-runs `v?/rca` with the same params and ships a 2-output
// payload: one stats row, N cause rows (each tagged with
// `is_precursor` derived from avg_lead_secs sign), and M cluster
// rows.  60/40 budget split in favour of causes — they directly
// answer the "what caused this?" question; clusters are
// supporting evidence for the causal story.

/// Same 60/40 split helper as `denoise_recent` / `knn`.
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
#[template(path = "partials/rca_analysis.html")]
struct RcaAnalysis {
    response:          String,
    response_html:     String,
    provider:          String,
    model:             String,
    ms:                u64,
    /// Failure key being analysed — echoed in the strip and panel
    /// header so the operator can see exactly what was investigated.
    /// Empty when the detector auto-picked the worst failure.
    failure_key:       String,
    /// Cause rows fed to the LLM (after the budget split).
    n_causes_fed:      usize,
    /// Cluster rows fed to the LLM (after the budget split).
    n_clusters_fed:    usize,
    /// Totals returned by v?/rca before the budget split.
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
    let cfg = state.rca_analyze.clone();
    let fk: Option<String> = if p.failure_key.is_empty() { None } else { Some(p.failure_key.clone()) };

    let resp = match rpc_versioned(&state, "v2/rca", "v3/rca", json!({
        "session":           SESSION,
        "duration":          p.duration.clone(),
        "failure_key":       fk,
        "bucket_secs":       p.bucket_secs,
        "min_support":       p.min_support,
        "jaccard_threshold": p.jaccard_threshold,
    })).await {
        Ok(v)  => v,
        Err(e) => return Ok(Html(RcaAnalysis {
            response:          String::new(),
            response_html:     String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:                0,
            failure_key:       p.failure_key.clone(),
            n_causes_fed:      0,
            n_clusters_fed:    0,
            matched_causes:    0,
            matched_clusters:  0,
            n_events:          0,
            n_keys:             0,
            duration:          p.duration.clone(),
            cache:    String::new(),
            error:    format!("Could not fetch RCA results for analysis: {e}"),
        }.render()?)),
    };

    // Pull raw arrays + stats.  Resolved failure key may differ from
    // the operator's input if it was empty (detector auto-picks).
    let resolved_failure_key = resp.get("failure_key")
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
        return Ok(Html(RcaAnalysis {
            response:          String::new(),
            response_html:     String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:                0,
            failure_key:       resolved_failure_key.clone(),
            n_causes_fed:      0,
            n_clusters_fed:    0,
            matched_causes:    0,
            matched_clusters:  0,
            n_events,
            n_keys,
            duration:          p.duration.clone(),
            cache:    String::new(),
            error:    format!(
                "RCA found no probable causes and no clusters in the last {} ({} event{}, {} \
                 key{} scanned) — try lowering `min_support` or `jaccard_threshold`, or \
                 widen the duration if the failure window is narrow.",
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

    // Stats row anchors the failure identification.
    rows.push(json!({
        "_kind":             "rca_window_stats",
        "failure_key":       resolved_failure_key,
        "failure_key_was_auto_picked": p.failure_key.is_empty(),
        "window_start":      start,
        "window_end":        end,
        "window_start_iso":  fmt_ts(start),
        "window_end_iso":    fmt_ts(end),
        "n_events":          n_events,
        "n_keys":            n_keys,
        "bucket_secs":       p.bucket_secs,
        "min_support":       p.min_support,
        "jaccard_threshold": p.jaccard_threshold,
        "duration":          p.duration,
        "n_causes_total":    matched_causes,
        "n_clusters_total":  matched_clusters,
        "n_causes_fed":      n_causes_fed,
        "n_clusters_fed":    n_clusters_fed,
    }));

    // Cause rows — add `_kind` + a synthetic `is_precursor` flag
    // derived from the lead sign so the LLM doesn't have to do
    // sign comparison inline.  Carry rank explicitly so the
    // prompt can reference "[cause 3]" / "[#3]".
    for (i, c) in causes_raw.iter().take(n_causes_fed).enumerate() {
        let lead_secs = c.get("avg_lead_secs").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let mut obj = c.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("_kind".into(),        json!("rca_cause"));
            m.insert("rank".into(),         json!(i + 1));
            m.insert("is_precursor".into(), json!(lead_secs >= 0.0));
        }
        rows.push(obj);
    }

    // Cluster rows — tag in place.  RCA cluster member lists are
    // typically small (few keys), so no separate clip is needed.
    for c in clusters_raw.iter().take(n_clusters_fed) {
        let mut obj = c.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("_kind".into(), json!("rca_cluster"));
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
            Ok(Html(RcaAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                failure_key:   resolved_failure_key,
                n_causes_fed, n_clusters_fed,
                matched_causes, matched_clusters,
                n_events, n_keys,
                duration: p.duration,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(RcaAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            failure_key:   resolved_failure_key,
            n_causes_fed, n_clusters_fed,
            matched_causes, matched_clusters,
            n_events, n_keys,
            duration: p.duration,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
