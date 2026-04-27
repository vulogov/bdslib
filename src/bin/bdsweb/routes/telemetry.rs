use askama::Template;
use axum::{
    extract::{Query, State},
    response::Html,
};
use serde::Deserialize;
use serde_json::json;

use crate::{client::{fmt_ts, rpc, SESSION}, error::AppError, state::AppState};

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default)]
    pub q: String,
}
fn default_duration() -> String { "1h".to_owned() }

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
    let ts    = v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
    let data  = v.get("data").map(|d| d.to_string()).unwrap_or_default();
    let score = v.get("_score").and_then(|x| x.as_f64())
                 .map(|f| format!("{f:.3}"))
                 .unwrap_or_else(|| "—".to_owned());
    HitRow {
        timestamp: fmt_ts(ts),
        key:       v.get("key").and_then(|x| x.as_str()).unwrap_or("—").to_owned(),
        data:      truncate(&data, 120),
        score,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_owned() } else { format!("{}…", &s[..n]) }
}

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "telemetry.html")]
struct TelemetryPage {
    duration: String,
    q:        String,
}

pub async fn page(Query(p): Query<Params>) -> Result<Html<String>, AppError> {
    let tmpl = TelemetryPage { duration: p.duration, q: p.q };
    Ok(Html(tmpl.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/telemetry_rows.html")]
struct TelemetryRows {
    rows:     Vec<HitRow>,
    duration: String,
    q:        String,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    if p.q.is_empty() {
        let tmpl = TelemetryRows { rows: vec![], duration: p.duration, q: p.q };
        return Ok(Html(tmpl.render()?));
    }

    let resp = rpc(&state, "v2/fulltext.get", json!({
        "session":  SESSION,
        "query":    p.q,
        "duration": p.duration,
    })).await?;

    let tmpl = TelemetryRows {
        rows:     to_rows(&resp["results"]),
        duration: p.duration,
        q:        p.q,
    };
    Ok(Html(tmpl.render()?))
}
