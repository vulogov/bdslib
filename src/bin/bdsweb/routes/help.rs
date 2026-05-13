//! `/help` — natural-language Q&A over the cluster docstore.
//!
//! Powered by [`v3/help`](../../../../Documentation/jsonrpc_api/v3_help.md).
//! The page hosts a single text field + Help! button + result pane;
//! the result is rendered with full markdown formatting (LLM output
//! is virtually always markdown) via `crate::markdown::render`
//! before being interpolated into the partial.

use askama::Template;
use axum::{extract::State, response::Html, Form};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{client::rpc_with_timeout, error::AppError, state::AppState};

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "help.html")]
struct HelpPage {}

pub async fn page() -> Result<Html<String>, AppError> {
    Ok(Html(HelpPage {}.render()?))
}

// ── POST /help/query ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct QueryForm {
    /// The English question.
    #[serde(default)]
    pub message: String,

    /// HTML form sends "on" when checked, absent when unchecked.
    /// The askama-friendly normalisation happens in the handler.
    #[serde(default)]
    pub internal_only: Option<String>,

    /// Caller-supplied limit; empty / non-numeric → default.
    #[serde(default)]
    pub limit: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/help_result.html")]
struct HelpResult {
    /// Server-rendered HTML of the LLM answer.  Escaped + sanitised
    /// by `markdown::render` (pulldown-cmark → ammonia allowlist).
    answer_html:    String,
    /// Raw text — used for the empty-check and the "copy answer"
    /// button's data attribute.
    answer:         String,
    n_docs:         u64,
    internal_only:  bool,
    limit:          u64,
    provider:       String,
    model:          String,
    ms:             u64,
    tokens_in:      u64,
    tokens_out:     u64,
    has_tokens:     bool,
    note:           String,
    has_note:       bool,
    sources:        Vec<SourceRow>,
    has_error:      bool,
    error_msg:      String,
}

struct SourceRow {
    name:         String,
    score:        String,
    internal_doc: bool,
}

pub async fn query(
    State(state): State<AppState>,
    Form(form): Form<QueryForm>,
) -> Result<Html<String>, AppError> {
    let message = form.message.trim();
    if message.is_empty() {
        return Ok(Html(render_error("Enter a question first.")?));
    }

    let internal_only = matches!(form.internal_only.as_deref(),
        Some("on") | Some("true") | Some("1"));
    let limit: Option<u64> = form.limit.as_deref()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0);

    let mut params = json!({
        "message":       message,
        "internal_only": internal_only,
    });
    if let Some(n) = limit {
        params["limit"] = json!(n);
    }

    // v3/help triggers an LLM completion — generous timeout (5 min)
    // matches what /chat allows for v4/llm.chat.
    let resp = match rpc_with_timeout(
        &state, "v3/help", params,
        Some(std::time::Duration::from_secs(300)),
    ).await {
        Ok(v) => v,
        Err(AppError::Rpc(msg)) => return Ok(Html(render_error(&msg)?)),
        Err(e) => return Err(e),
    };

    Ok(Html(build_partial(&resp)?))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn render_error(msg: &str) -> Result<String, AppError> {
    Ok(HelpResult {
        answer_html: String::new(), answer: String::new(),
        n_docs: 0, internal_only: false, limit: 0,
        provider: String::new(), model: String::new(),
        ms: 0, tokens_in: 0, tokens_out: 0, has_tokens: false,
        note: String::new(), has_note: false,
        sources: Vec::new(),
        has_error: true, error_msg: msg.to_owned(),
    }.render()?)
}

fn build_partial(resp: &Value) -> Result<String, AppError> {
    let answer  = resp.get("answer").and_then(Value::as_str).unwrap_or("").to_owned();
    let n_docs  = resp.get("n_docs").and_then(Value::as_u64).unwrap_or(0);
    let internal_only = resp.get("internal_only").and_then(Value::as_bool).unwrap_or(false);
    let limit   = resp.get("limit").and_then(Value::as_u64).unwrap_or(0);
    let provider = resp.get("provider").and_then(Value::as_str).unwrap_or("").to_owned();
    let model    = resp.get("model").and_then(Value::as_str).unwrap_or("").to_owned();
    let ms       = resp.get("ms").and_then(Value::as_u64).unwrap_or(0);
    let tokens_in  = resp.get("tokens_in").and_then(Value::as_u64).unwrap_or(0);
    let tokens_out = resp.get("tokens_out").and_then(Value::as_u64).unwrap_or(0);
    let has_tokens = tokens_in > 0 || tokens_out > 0;
    let note     = resp.get("note").and_then(Value::as_str).unwrap_or("").to_owned();
    let has_note = !note.is_empty();

    let sources = resp.get("sources").and_then(Value::as_array)
        .map(|arr| arr.iter().map(source_row).collect::<Vec<_>>())
        .unwrap_or_default();

    let answer_html = crate::markdown::render(&answer);

    Ok(HelpResult {
        answer_html, answer,
        n_docs, internal_only, limit,
        provider, model, ms, tokens_in, tokens_out, has_tokens,
        note, has_note, sources,
        has_error: false, error_msg: String::new(),
    }.render()?)
}

fn source_row(v: &Value) -> SourceRow {
    let name  = v.get("name").and_then(Value::as_str).unwrap_or("<no name>").to_owned();
    let score = v.get("score").and_then(Value::as_f64).unwrap_or(0.0);
    let internal_doc = v.get("internal_doc").and_then(Value::as_bool).unwrap_or(false);
    SourceRow {
        name,
        score: format!("{score:.3}"),
        internal_doc,
    }
}
