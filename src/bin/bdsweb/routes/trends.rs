use askama::Template;
use axum::{extract::{Query, State}, response::Html};
use serde::Deserialize;
use serde_json::json;

use crate::{client::{rpc, SESSION}, error::AppError, state::AppState};

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default)]
    pub key: String,
    #[serde(default = "default_duration")]
    pub duration: String,
}
fn default_duration() -> String { "1h".to_owned() }

#[derive(Debug, Default)]
pub struct TrendStats {
    pub n:           usize,
    pub min:         String,
    pub max:         String,
    pub mean:        String,
    pub median:      String,
    pub std_dev:     String,
    pub variability: String,
    pub anomalies:   usize,
    pub breakouts:   usize,
}

fn extract_stats(v: &serde_json::Value) -> TrendStats {
    let f = |key: &str| -> String {
        v.get(key).and_then(|x| x.as_f64())
         .map(|f| format!("{f:.4}"))
         .unwrap_or_else(|| "—".to_owned())
    };
    TrendStats {
        n:           v.get("n").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
        min:         f("min"),
        max:         f("max"),
        mean:        f("mean"),
        median:      f("median"),
        std_dev:     f("std_dev"),
        variability: f("variability"),
        anomalies:   v.get("anomalies").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0),
        breakouts:   v.get("breakouts").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0),
    }
}

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "trends.html")]
struct TrendsPage { key: String, duration: String }

pub async fn page(Query(p): Query<Params>) -> Result<Html<String>, AppError> {
    Ok(Html(TrendsPage { key: p.key, duration: p.duration }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/trends_data.html")]
struct TrendsData {
    key:            String,
    duration:       String,
    stats:          TrendStats,
    uplot_data_json: String,
    has_data:       bool,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    if p.key.is_empty() {
        return Ok(Html(TrendsData {
            key: p.key, duration: p.duration,
            stats: TrendStats::default(),
            uplot_data_json: "[[],[]]".to_owned(),
            has_data: false,
        }.render()?));
    }

    let (trend_v, telemetry_v) = tokio::try_join!(
        rpc(&state, "v2/trends", json!({
            "session":  SESSION,
            "key":      p.key,
            "duration": p.duration,
        })),
        rpc(&state, "v2/primaries.get.telemetry", json!({
            "session":  SESSION,
            "key":      p.key,
            "duration": p.duration,
        })),
    )?;

    let stats = extract_stats(&trend_v);

    let results_arr = telemetry_v.get("results")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();

    let mut timestamps: Vec<f64> = Vec::with_capacity(results_arr.len());
    let mut values:     Vec<f64> = Vec::with_capacity(results_arr.len());
    for pt in &results_arr {
        if let (Some(ts), Some(val)) = (
            pt.get("timestamp").and_then(|x| x.as_u64()),
            pt.get("value").and_then(|x| x.as_f64()),
        ) {
            timestamps.push(ts as f64);
            values.push(val);
        }
    }

    let uplot_data_json = serde_json::to_string(&[&timestamps, &values])?;
    let has_data = !timestamps.is_empty();

    Ok(Html(TrendsData {
        key:             p.key,
        duration:        p.duration,
        stats,
        uplot_data_json,
        has_data,
    }.render()?))
}
