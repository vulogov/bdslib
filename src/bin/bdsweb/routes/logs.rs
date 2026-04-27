use askama::Template;
use axum::{extract::{Query, State}, response::Html};
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
pub struct LogRow {
    pub timestamp: String,
    pub key:       String,
    pub message:   String,
    pub score:     String,
}

fn to_rows(results: &serde_json::Value) -> Vec<LogRow> {
    results.as_array()
           .map(|arr| arr.iter().map(hit_to_row).collect())
           .unwrap_or_default()
}

fn hit_to_row(v: &serde_json::Value) -> LogRow {
    let ts = v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
    let data = v.get("data");
    let message = data
        .and_then(|d| d.as_str())
        .map(|s| s.to_owned())
        .or_else(|| data.and_then(|d| d.get("message")).and_then(|m| m.as_str()).map(|s| s.to_owned()))
        .or_else(|| data.map(|d| d.to_string()))
        .unwrap_or_default();
    let score = v.get("_score").and_then(|x| x.as_f64())
                 .map(|f| format!("{f:.3}"))
                 .unwrap_or_else(|| "—".to_owned());
    LogRow {
        timestamp: fmt_ts(ts),
        key:       v.get("key").and_then(|x| x.as_str()).unwrap_or("—").to_owned(),
        message:   truncate(&message, 160),
        score,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_owned() } else { format!("{}…", &s[..n]) }
}

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "logs.html")]
struct LogsPage { duration: String, q: String }

pub async fn page(Query(p): Query<Params>) -> Result<Html<String>, AppError> {
    Ok(Html(LogsPage { duration: p.duration, q: p.q }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/log_rows.html")]
struct LogRows { rows: Vec<LogRow>, duration: String, q: String }

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    if p.q.is_empty() {
        return Ok(Html(LogRows { rows: vec![], duration: p.duration, q: p.q }.render()?));
    }

    let resp = rpc(&state, "v2/fulltext.get", json!({
        "session":  SESSION,
        "query":    p.q,
        "duration": p.duration,
    })).await?;

    Ok(Html(LogRows {
        rows:     to_rows(&resp["results"]),
        duration: p.duration,
        q:        p.q,
    }.render()?))
}
