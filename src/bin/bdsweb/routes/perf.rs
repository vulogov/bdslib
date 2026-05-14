//! `/perf` — performance dashboard backed by `v2/perf` and
//! `v2/perf.slow_queries`.  Shell + data + refresh follow the
//! cluster/dashboard pattern.
//!
//! No background poller — the page is "diagnostic": operators visit it
//! when investigating something specific.  Auto-refresh is driven by
//! HTMX polling.

use askama::Template;
use axum::{extract::State, response::Html};
use serde_json::{json, Value};

use crate::{
    admin::signed_rpc_with_timeout,
    client::{rpc, fmt_ts},
    error::AppError,
    state::AppState,
};

// ── Shell ───────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "perf.html")]
struct PerfShell {
    refresh_secs:     u64,
    /// Default LLM provider id surfaced for the "Analyze this!" wait
    /// banner.  Empty when bdsnode has no providers registered or
    /// HMAC isn't configured — the JS degrades to a generic message.
    analyze_provider: String,
    /// Default model for the same provider; same fallback rules.
    analyze_model:    String,
}

/// Pull the default LLM provider + its default model from
/// `v4/llm.providers.list` so the wait banner can name the actual
/// upstream.  Mirrors the same helper in routes::logs.  All failures
/// collapse to `("", "")` and the page falls back to a generic message.
async fn fetch_analyze_provider(state: &AppState) -> (String, String) {
    if state.shared_secret.is_empty() {
        return (String::new(), String::new());
    }
    let resp = match crate::admin::signed_rpc(
        state, "v4/llm.providers.list", json!({})).await
    {
        Ok(v)  => v,
        Err(e) => {
            log::warn!("[perf] v4/llm.providers.list failed: {e}");
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

pub async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    // Reuse the dashboard refresh cadence — perf data updates as
    // frequently as the dashboard's underlying v2/status snapshot.
    let (analyze_provider, analyze_model) = fetch_analyze_provider(&state).await;
    Ok(Html(PerfShell {
        refresh_secs:     state.dashboard_refresh_secs,
        analyze_provider,
        analyze_model,
    }.render()?))
}

// ── Data partial ────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SeriesRow {
    pub name:     String,
    pub n_total:  u64,
    pub n_recent: u64,
    pub min_us:   u64,
    pub max_us:   u64,
    pub p50_us:   u64,
    pub p95_us:   u64,
    pub p99_us:   u64,
}

#[derive(Debug)]
pub struct SlowRow {
    pub name:       String,
    pub elapsed_ms: u64,
    pub when:       String,
}

#[derive(Template)]
#[template(path = "partials/perf_data.html")]
struct PerfData {
    series:            Vec<SeriesRow>,
    slow_entries:      Vec<SlowRow>,
    slow_threshold_ms: u64,
}

fn parse_series(v: &Value) -> Vec<SeriesRow> {
    let Some(obj) = v.as_object() else { return Vec::new() };
    let mut out: Vec<SeriesRow> = obj.iter().map(|(name, s)| SeriesRow {
        name:     name.clone(),
        n_total:  s.get("n_total").and_then(|x| x.as_u64()).unwrap_or(0),
        n_recent: s.get("n_recent").and_then(|x| x.as_u64()).unwrap_or(0),
        min_us:   s.get("min_us").and_then(|x| x.as_u64()).unwrap_or(0),
        max_us:   s.get("max_us").and_then(|x| x.as_u64()).unwrap_or(0),
        p50_us:   s.get("p50_us").and_then(|x| x.as_u64()).unwrap_or(0),
        p95_us:   s.get("p95_us").and_then(|x| x.as_u64()).unwrap_or(0),
        p99_us:   s.get("p99_us").and_then(|x| x.as_u64()).unwrap_or(0),
    }).collect();
    // Sort by p95 desc — the most useful order for spotting issues.
    out.sort_by(|a, b| b.p95_us.cmp(&a.p95_us).then_with(|| a.name.cmp(&b.name)));
    out
}

fn parse_slow(v: &Value) -> (u64, Vec<SlowRow>) {
    let threshold_ms = v.get("threshold_ms").and_then(|x| x.as_u64()).unwrap_or(0);
    let entries: Vec<SlowRow> = v.get("entries")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().map(|e| SlowRow {
            name:       e.get("name").and_then(|x| x.as_str()).unwrap_or("").to_owned(),
            elapsed_ms: e.get("elapsed_ms").and_then(|x| x.as_u64()).unwrap_or(0),
            when:       fmt_ts(e.get("ts").and_then(|x| x.as_u64()).unwrap_or(0)),
        }).collect())
        .unwrap_or_default();
    (threshold_ms, entries)
}

pub async fn data(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    // Two RPCs in parallel — both are pure in-process snapshots so the
    // cost is essentially the JSON marshalling.
    let (perf, slow) = tokio::try_join!(
        rpc(&state, "v2/perf",              json!({})),
        rpc(&state, "v2/perf.slow_queries", json!({})),
    )?;

    let series = parse_series(&perf);
    let (slow_threshold_ms, slow_entries) = parse_slow(&slow);

    Ok(Html(PerfData {
        series,
        slow_entries,
        slow_threshold_ms,
    }.render()?))
}

// ── HTMX: "Analyze this!" — one-shot LLM analysis of current perf snapshot ───
//
// Re-fetches `v2/perf` + `v2/perf.slow_queries`, normalises the two
// blocks into a single `rows` array (typed via a `kind` discriminator
// so the model can tell series-summary from slow-log entries), and
// hands it to `v4/llm.analyze` with the perf-focused prompt template
// from `web.analyze.perf.prompt_template`.
//
// One-shot — no chat history — same pattern as logs/metrics analyze.

#[derive(Template)]
#[template(path = "partials/perf_analysis.html")]
struct PerfAnalysis {
    response:      String,
    response_html: String,
    provider:      String,
    model:         String,
    ms:            u64,
    n_rows:        usize,
    cache:         String,
    error:         String,
}

pub async fn analyze(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let cfg = state.perf_analyze.clone();

    // Pull both perf blocks in parallel — same pattern as `data` above.
    let (perf, slow) = match tokio::try_join!(
        rpc(&state, "v2/perf",              json!({})),
        rpc(&state, "v2/perf.slow_queries", json!({})),
    ) {
        Ok(t)  => t,
        Err(e) => return Ok(Html(PerfAnalysis {
            response: String::new(), response_html: String::new(),
            provider: String::new(), model: String::new(),
            ms: 0, n_rows: 0, cache: String::new(),
            error: format!("Could not fetch perf data: {e}"),
        }.render()?)),
    };

    // Build a typed `rows` payload so the model sees BOTH series and
    // slow-log entries with a clear discriminator.  Each row carries
    // a `kind` field — "series" vs "slow_entry" — and the relevant
    // numbers / labels.  The full prompt explains the taxonomy.
    let mut rows: Vec<Value> = Vec::with_capacity(cfg.max_rows);

    if let Some(obj) = perf.as_object() {
        // Series rows first, sorted by p95 desc so the head of the
        // array is what the model cares about most when truncated.
        let mut series: Vec<(&String, &Value)> = obj.iter().collect();
        series.sort_by(|a, b| {
            let pa = a.1.get("p95_us").and_then(|v| v.as_u64()).unwrap_or(0);
            let pb = b.1.get("p95_us").and_then(|v| v.as_u64()).unwrap_or(0);
            pb.cmp(&pa).then_with(|| a.0.cmp(b.0))
        });
        for (name, s) in series {
            rows.push(json!({
                "kind":     "series",
                "name":     name,
                "n_total":  s.get("n_total").and_then(|v| v.as_u64()).unwrap_or(0),
                "n_recent": s.get("n_recent").and_then(|v| v.as_u64()).unwrap_or(0),
                "min_us":   s.get("min_us").and_then(|v| v.as_u64()).unwrap_or(0),
                "p50_us":   s.get("p50_us").and_then(|v| v.as_u64()).unwrap_or(0),
                "p95_us":   s.get("p95_us").and_then(|v| v.as_u64()).unwrap_or(0),
                "p99_us":   s.get("p99_us").and_then(|v| v.as_u64()).unwrap_or(0),
                "max_us":   s.get("max_us").and_then(|v| v.as_u64()).unwrap_or(0),
                "mean_us":  s.get("mean_us").and_then(|v| v.as_u64()).unwrap_or(0),
            }));
        }
    }

    let slow_threshold_ms = slow.get("threshold_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    if let Some(arr) = slow.get("entries").and_then(|v| v.as_array()) {
        for e in arr {
            rows.push(json!({
                "kind":         "slow_entry",
                "name":         e.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "elapsed_us":   e.get("elapsed_us").and_then(|v| v.as_u64()).unwrap_or(0),
                "elapsed_ms":   e.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                "ts":           e.get("ts").and_then(|v| v.as_u64()).unwrap_or(0),
                "threshold_ms": slow_threshold_ms,
            }));
        }
    }

    // Enforce the operator-configurable cap.  Series are already
    // sorted by p95 desc, then slow entries follow newest-first, so
    // truncation drops the tail — the least-interesting series and
    // the oldest slow entries.
    rows.truncate(cfg.max_rows);
    let n_rows = rows.len();

    if n_rows == 0 {
        return Ok(Html(PerfAnalysis {
            response: String::new(), response_html: String::new(),
            provider: String::new(), model: String::new(),
            ms: 0, n_rows: 0, cache: String::new(),
            error: "No perf data recorded yet — drive some traffic through the node first.".to_owned(),
        }.render()?));
    }

    // The `query` field is set to a stable label so the cache key is
    // useful — re-analyzing the same snapshot returns a cache hit.
    // (rows are part of the cache key too; identical perf data won't
    // re-run inference.)
    let analyze_resp = signed_rpc_with_timeout(
        &state,
        "v4/llm.analyze",
        json!({
            "kind":            "supplied",
            "rows":            rows,
            "query":           "perf snapshot",
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
            Ok(Html(PerfAnalysis {
                response_html: crate::markdown::render(&response),
                response,
                provider, model, ms,
                n_rows,
                cache,
                error: String::new(),
            }.render()?))
        }
        Err(e) => Ok(Html(PerfAnalysis {
            response: String::new(), response_html: String::new(),
            provider: String::new(), model: String::new(),
            ms: 0, n_rows, cache: String::new(),
            error: format!("v4/llm.analyze failed: {e}"),
        }.render()?)),
    }
}
