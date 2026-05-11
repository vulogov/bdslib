use askama::Template;
use axum::{extract::{Query, State}, response::Html};
use serde::Deserialize;
use serde_json::json;

use crate::{client::{mode_badge_for_page, ModeBadge, rpc_versioned, SESSION}, error::AppError, state::AppState};

// ── Query parameters ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct Params {
    #[serde(default = "default_duration")]
    pub duration: String,
    #[serde(default)]
    pub max_sentences: usize,
    #[serde(default = "default_min_word_len")]
    pub min_word_len: usize,
}
fn default_duration()    -> String { "1h".to_owned() }
fn default_min_word_len() -> usize { 2 }

// ── Full page (shell) ─────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "primary_summary.html")]
struct PrimarySummaryPage {
    duration:      String,
    max_sentences: usize,
    min_word_len:  usize,
    mode_badge:    ModeBadge,
}

pub async fn page(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let mode_badge = mode_badge_for_page(&state, true).await;
    Ok(Html(PrimarySummaryPage {
        duration:      p.duration,
        max_sentences: p.max_sentences,
        min_word_len:  p.min_word_len,
        mode_badge,
    }.render()?))
}

// ── HTMX results fragment ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/primary_summary_result.html")]
struct PrimarySummaryResult {
    duration:      String,
    max_sentences: usize,
    summary:       String,
    has_summary:   bool,
}

pub async fn results(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let resp = rpc_versioned(&state, "v2/summary_for_recent", "v3/summary_for_recent", json!({
        "session":       SESSION,
        "duration":      p.duration,
        "max_sentences": p.max_sentences,
        "min_word_len":  p.min_word_len,
    })).await?;

    let summary = resp.get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let has_summary = !summary.is_empty();

    Ok(Html(PrimarySummaryResult {
        duration:      p.duration,
        max_sentences: p.max_sentences,
        summary,
        has_summary,
    }.render()?))
}
