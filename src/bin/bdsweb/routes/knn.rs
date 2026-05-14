use askama::Template;
use axum::{extract::{Form, Query, State}, response::Html};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

use crate::{admin::signed_rpc_with_timeout, client::{mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default = "default_k")]
    pub k: usize,
    #[serde(default = "default_min_word_len")]
    pub min_word_len: usize,
    #[serde(default = "default_anomaly_threshold")]
    pub anomaly_threshold: f32,
    #[serde(default = "default_max_cluster_members")]
    pub max_cluster_members: usize,
    #[serde(default = "default_max_anomalies")]
    pub max_anomalies: usize,
}
fn default_duration()            -> String { "1h".to_owned() }
fn default_k()                   -> usize { 5 }
fn default_min_word_len()        -> usize { 2 }
fn default_anomaly_threshold()   -> f32   { 0.2 }
fn default_max_cluster_members() -> usize { 10 }
fn default_max_anomalies()       -> usize { 20 }

// ── Page shell ────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "knn.html")]
struct KnnPage {
    duration:            String,
    k:                   usize,
    min_word_len:        usize,
    anomaly_threshold:   f32,
    max_cluster_members: usize,
    max_anomalies:       usize,
    mode_badge:          ModeBadge,
    /// Default LLM provider id, surfaced on `data-` for the
    /// wait-message JS.  Empty when bdsnode reports no providers.
    analyze_provider:    String,
    /// Default model name.  Empty when unavailable.
    analyze_model:       String,
}

pub async fn page(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let (mode_badge, (analyze_provider, analyze_model)) = tokio::join!(
        mode_badge_for_page(&state, true),
        crate::client::analyze_provider(&state),
    );
    Ok(Html(KnnPage {
        duration:            p.duration,
        k:                   p.k,
        min_word_len:        p.min_word_len,
        anomaly_threshold:   p.anomaly_threshold,
        max_cluster_members: p.max_cluster_members,
        max_anomalies:       p.max_anomalies,
        mode_badge,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ClusterMember {
    pub idx:     u64,
    pub density: f64,
    pub text:    String,
}

#[derive(Debug)]
pub struct ClusterRow {
    pub id:               u64,
    pub size:             u64,
    pub rep_idx:          u64,
    pub rep_density:      f64,
    pub rep_text:         String,
    pub rep_short:        String,
    pub members:          Vec<ClusterMember>,
    pub members_shown:    usize,
}

#[derive(Debug)]
pub struct AnomalyRow {
    pub idx:            u64,
    pub max_similarity: f64,
    pub text:           String,
}

#[derive(Template)]
#[template(path = "partials/knn_result.html")]
struct KnnResult {
    duration:            String,
    n_logs:              u64,
    k:                   u64,
    anomaly_threshold:   f64,
    n_clusters:          u64,
    n_anomalies:         u64,
    has_clusters:        bool,
    has_anomalies:       bool,
    clusters:            Vec<ClusterRow>,
    anomalies:           Vec<AnomalyRow>,
    mode_badge:          ModeBadge,
}

/// First 80 characters of `s` with an ellipsis appended when truncated —
/// keeps the cluster header card readable when the representative
/// fingerprint is very long.
fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let resp = rpc_versioned(&state, "v2/knn", "v3/knn", json!({
        "session":             SESSION,
        "duration":            p.duration.clone(),
        "k":                   p.k,
        "min_word_len":        p.min_word_len,
        "anomaly_threshold":   p.anomaly_threshold,
        "max_cluster_members": p.max_cluster_members,
        "max_anomalies":       p.max_anomalies,
    })).await?;

    let n_logs            = resp.get("n_logs").and_then(JsonValue::as_u64).unwrap_or(0);
    let k_eff             = resp.get("k").and_then(JsonValue::as_u64).unwrap_or(p.k as u64);
    let anomaly_threshold = resp.get("anomaly_threshold").and_then(JsonValue::as_f64)
        .unwrap_or(p.anomaly_threshold as f64);
    let n_clusters        = resp.get("n_clusters").and_then(JsonValue::as_u64).unwrap_or(0);
    let n_anomalies       = resp.get("n_anomalies").and_then(JsonValue::as_u64).unwrap_or(0);

    // Clusters: for each, extract id/size/representative/members.
    let clusters: Vec<ClusterRow> = resp.get("clusters")
        .and_then(JsonValue::as_array)
        .map(|arr| arr.iter().map(|c| {
            let rep = c.get("representative").cloned().unwrap_or(JsonValue::Null);
            let rep_text = rep.get("text").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let members: Vec<ClusterMember> = c.get("members")
                .and_then(JsonValue::as_array)
                .map(|ms| ms.iter().map(|m| ClusterMember {
                    idx:     m.get("idx").and_then(JsonValue::as_u64).unwrap_or(0),
                    density: m.get("density").and_then(JsonValue::as_f64).unwrap_or(0.0),
                    text:    m.get("text").and_then(|v| v.as_str()).unwrap_or("").to_owned(),
                }).collect())
                .unwrap_or_default();
            let members_shown = members.len();
            ClusterRow {
                id:           c.get("id").and_then(JsonValue::as_u64).unwrap_or(0),
                size:         c.get("size").and_then(JsonValue::as_u64).unwrap_or(0),
                rep_idx:      rep.get("idx").and_then(JsonValue::as_u64).unwrap_or(0),
                rep_density:  rep.get("density").and_then(JsonValue::as_f64).unwrap_or(0.0),
                rep_short:    shorten(&rep_text, 100),
                rep_text,
                members,
                members_shown,
            }
        }).collect())
        .unwrap_or_default();

    let anomalies: Vec<AnomalyRow> = resp.get("anomalies")
        .and_then(JsonValue::as_array)
        .map(|arr| arr.iter().map(|a| AnomalyRow {
            idx:            a.get("idx").and_then(JsonValue::as_u64).unwrap_or(0),
            max_similarity: a.get("max_similarity").and_then(JsonValue::as_f64).unwrap_or(0.0),
            text:           a.get("text").and_then(|v| v.as_str()).unwrap_or("").to_owned(),
        }).collect())
        .unwrap_or_default();

    let has_clusters  = !clusters.is_empty();
    let has_anomalies = !anomalies.is_empty();
    let mode_badge    = ModeBadge::from_response(&resp);

    Ok(Html(KnnResult {
        duration:          p.duration,
        n_logs,
        k:                 k_eff,
        anomaly_threshold,
        n_clusters,
        n_anomalies,
        has_clusters,
        has_anomalies,
        clusters,
        anomalies,
        mode_badge,
    }.render()?))
}

// ── HTMX: "Analyze this!" — interpret the clustering structure ───────────────
//
// Re-runs `v?/knn`, then ships a 2-output payload: one stats row,
// N cluster rows (each carrying id, size, representative, and a
// clipped members list), and M anomaly rows.  The 60/40 budget
// split favours clusters since each cluster row carries far more
// info per row than a single anomaly.

/// Per-cluster cap on member rows fed verbatim to the LLM.  A
/// 200-member heartbeat cluster doesn't need 200 verbatim quotes
/// — 5 is enough to characterise the theme.
const KNN_MEMBERS_PER_CLUSTER: usize = 5;

/// Same 60/40 split helper as `denoise_recent::split_two_corpus_budget`.
/// Slack from either side redistributes so the full budget is always
/// used when there's enough supply.
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
#[template(path = "partials/knn_analysis.html")]
struct KnnAnalysis {
    response:          String,
    response_html:     String,
    provider:          String,
    model:             String,
    ms:                u64,
    /// Cluster rows fed to the LLM (after the budget split).
    n_clusters_fed:    usize,
    /// Anomaly rows fed to the LLM.
    n_anomalies_fed:   usize,
    /// Totals returned by v?/knn before the budget split.
    matched_clusters:  usize,
    matched_anomalies: usize,
    n_logs:            u64,
    k:                 u64,
    anomaly_threshold: f64,
    duration:          String,
    /// `"miss"`, `"hit"`, or `""`.
    cache:             String,
    /// Empty when the LLM ran cleanly; banner message otherwise.
    error:             String,
}

pub async fn analyze(
    State(state): State<AppState>,
    Form(p): Form<Params>,
) -> Result<Html<String>, AppError> {
    let cfg = state.knn_analyze.clone();

    let resp = match rpc_versioned(&state, "v2/knn", "v3/knn", json!({
        "session":             SESSION,
        "duration":            p.duration.clone(),
        "k":                   p.k,
        "min_word_len":        p.min_word_len,
        "anomaly_threshold":   p.anomaly_threshold,
        "max_cluster_members": p.max_cluster_members,
        "max_anomalies":       p.max_anomalies,
    })).await {
        Ok(v)  => v,
        Err(e) => return Ok(Html(KnnAnalysis {
            response:          String::new(),
            response_html:     String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:                0,
            n_clusters_fed:    0,
            n_anomalies_fed:   0,
            matched_clusters:  0,
            matched_anomalies: 0,
            n_logs:            0,
            k:                 p.k as u64,
            anomaly_threshold: p.anomaly_threshold as f64,
            duration:          p.duration.clone(),
            cache:    String::new(),
            error:    format!("Could not fetch k-NN results for analysis: {e}"),
        }.render()?)),
    };

    let n_logs          = resp.get("n_logs").and_then(JsonValue::as_u64).unwrap_or(0);
    let k_eff           = resp.get("k").and_then(JsonValue::as_u64).unwrap_or(p.k as u64);
    let threshold_used  = resp.get("anomaly_threshold").and_then(JsonValue::as_f64)
        .unwrap_or(p.anomaly_threshold as f64);
    let clusters_raw: Vec<JsonValue> = resp.get("clusters")
        .and_then(JsonValue::as_array).cloned().unwrap_or_default();
    let anomalies_raw: Vec<JsonValue> = resp.get("anomalies")
        .and_then(JsonValue::as_array).cloned().unwrap_or_default();
    let matched_clusters  = clusters_raw.len();
    let matched_anomalies = anomalies_raw.len();

    if matched_clusters == 0 && matched_anomalies == 0 {
        return Ok(Html(KnnAnalysis {
            response:          String::new(),
            response_html:     String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:                0,
            n_clusters_fed:    0,
            n_anomalies_fed:   0,
            matched_clusters:  0,
            matched_anomalies: 0,
            n_logs,
            k:                 k_eff,
            anomaly_threshold: threshold_used,
            duration:          p.duration.clone(),
            cache:    String::new(),
            error:    format!(
                "k-NN found no clusters and no anomalies in the last {} ({} record{} \
                 scanned) — widen the duration or check that primary ingestion is \
                 emitting enough records for clustering.",
                p.duration, n_logs, if n_logs == 1 { "" } else { "s" }
            ),
        }.render()?));
    }

    // 60/40 split between clusters and anomalies — clusters carry
    // denser info per row (each is a representative + members list).
    let (n_clusters_fed, n_anomalies_fed) = split_two_corpus_budget(
        matched_clusters, matched_anomalies, cfg.max_rows,
    );

    let mut rows: Vec<JsonValue> = Vec::with_capacity(n_clusters_fed + n_anomalies_fed + 1);

    // Stats row first — anchors population context.
    rows.push(json!({
        "_kind":             "knn_window_stats",
        "n_logs":            n_logs,
        "k":                 k_eff,
        "anomaly_threshold": threshold_used,
        "n_clusters_total":  matched_clusters,
        "n_anomalies_total": matched_anomalies,
        "n_clusters_fed":    n_clusters_fed,
        "n_anomalies_fed":   n_anomalies_fed,
        "duration":          p.duration,
    }));

    // Cluster rows — project to {id, size, representative, members[..N]}
    // with members capped at KNN_MEMBERS_PER_CLUSTER so one 200-member
    // heartbeat cluster doesn't crowd the others out.
    for c in clusters_raw.iter().take(n_clusters_fed) {
        let id   = c.get("id").and_then(JsonValue::as_u64).unwrap_or(0);
        let size = c.get("size").and_then(JsonValue::as_u64).unwrap_or(0);
        let rep  = c.get("representative").cloned().unwrap_or(JsonValue::Null);
        let members_clipped: Vec<JsonValue> = c.get("members")
            .and_then(JsonValue::as_array)
            .map(|ms| ms.iter().take(KNN_MEMBERS_PER_CLUSTER).cloned().collect())
            .unwrap_or_default();
        let members_total = c.get("members").and_then(JsonValue::as_array)
            .map(|ms| ms.len()).unwrap_or(0);
        rows.push(json!({
            "_kind":            "knn_cluster",
            "id":               id,
            "size":             size,
            "representative":   rep,
            "members":          members_clipped,
            "members_total":    members_total,
            "members_in_payload": members_clipped.len(),
        }));
    }

    // Anomaly rows — tag in place.
    for a in anomalies_raw.iter().take(n_anomalies_fed) {
        let mut obj = a.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("_kind".into(), json!("knn_anomaly"));
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
            Ok(Html(KnnAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                n_clusters_fed, n_anomalies_fed,
                matched_clusters, matched_anomalies,
                n_logs,
                k:                 k_eff,
                anomaly_threshold: threshold_used,
                duration: p.duration,
                cache,
                error:    String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(KnnAnalysis {
            response:      String::new(),
            response_html: String::new(),
            provider: String::new(),
            model:    String::new(),
            ms:       0,
            n_clusters_fed, n_anomalies_fed,
            matched_clusters, matched_anomalies,
            n_logs,
            k:                 k_eff,
            anomaly_threshold: threshold_used,
            duration: p.duration,
            cache:    String::new(),
            error:    format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
