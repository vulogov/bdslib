use askama::Template;
use axum::{extract::State, response::Html, Form};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{client::{rpc, ModeBadge}, error::AppError,
            error_pretty::{parse_bund_error, ErrorSegment},
            state::AppState};

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "bund.html")]
struct BundPage {}

pub async fn page() -> Result<Html<String>, AppError> {
    Ok(Html(BundPage {}.render()?))
}

// ── Eval POST ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct EvalForm {
    #[serde(default)]
    context: String,
    #[serde(default)]
    script: String,
}

#[derive(Template)]
#[template(path = "partials/bund_result.html")]
struct BundResult {
    results:           Vec<String>,
    error_msg:         String,
    /// Same error as `error_msg`, but parsed into typed segments so
    /// the template can render token / reason / source / text spans
    /// with distinct colors.  Empty when `has_error` is false.
    error_segments:    Vec<ErrorSegment>,
    has_error:         bool,
    /// Some(badge) when the script's most-recent cls.* call left a
    /// cluster_meta on the per-thread cell.  None means either no
    /// cluster-aware helper ran, or the last one was local-only —
    /// either way no badge is rendered.
    cluster_badge:     Option<ModeBadge>,
    /// Pretty-printed cluster_meta JSON shown collapsed under the badge.
    /// Empty when `cluster_badge` is None.
    cluster_meta_json: String,
}

pub async fn eval(
    State(state): State<AppState>,
    Form(form): Form<EvalForm>,
) -> Result<Html<String>, AppError> {
    if form.script.trim().is_empty() {
        return Ok(Html(BundResult {
            results: vec![],
            error_msg: String::new(),
            error_segments: vec![],
            has_error: false,
            cluster_badge: None,
            cluster_meta_json: String::new(),
        }.render()?));
    }

    let ctx = if form.context.trim().is_empty() {
        "default".to_owned()
    } else {
        form.context.clone()
    };

    match rpc(&state, "v2/eval", json!({ "context": ctx, "script": form.script })).await {
        Ok(v) => {
            let results = match v.get("result") {
                None | Some(serde_json::Value::Null) => vec![],
                Some(r) => vec![
                    serde_json::to_string_pretty(r).unwrap_or_else(|_| r.to_string())
                ],
            };
            // Only show a badge when the script actually ran a cls.* call —
            // if `cluster_meta` is null (no helper ran, or the last one was
            // local-only) we suppress the badge so plain db.*/doc.* scripts
            // don't grow visual noise.
            let (cluster_badge, cluster_meta_json) = match v.get("cluster_meta") {
                Some(meta) if meta.is_object() => (
                    Some(ModeBadge::from_response(&v)),
                    serde_json::to_string_pretty(meta).unwrap_or_default(),
                ),
                _ => (None, String::new()),
            };
            Ok(Html(BundResult {
                results, error_msg: String::new(), error_segments: vec![],
                has_error: false,
                cluster_badge, cluster_meta_json,
            }.render()?))
        }
        Err(AppError::Rpc(msg)) => {
            let error_segments = parse_bund_error(&msg);
            Ok(Html(BundResult {
                results: vec![], error_msg: msg, error_segments,
                has_error: true,
                cluster_badge: None, cluster_meta_json: String::new(),
            }.render()?))
        }
        Err(e) => Err(e),
    }
}

// ── English → Bund translate POST ────────────────────────────────────────────
//
// Posts a plain English message to `v2/to.bund` and renders the
// returned script + metadata so the page can offer "Use as script"
// to drop it into the CodeMirror editor.

#[derive(Deserialize)]
pub struct TranslateForm {
    #[serde(default)]
    pub message: String,
}

#[derive(Template)]
#[template(path = "partials/bund_translate.html")]
struct BundTranslate {
    script:         String,
    valid:          bool,
    parse_attempts: u64,
    parse_error:    String,
    provider:       String,
    model:          String,
    ms:             u64,
    has_error:      bool,
    error_msg:      String,
}

pub async fn translate(
    State(state): State<AppState>,
    Form(form): Form<TranslateForm>,
) -> Result<Html<String>, AppError> {
    let message = form.message.trim();
    if message.is_empty() {
        return Ok(Html(BundTranslate {
            script: String::new(), valid: false, parse_attempts: 0,
            parse_error: String::new(),
            provider: String::new(), model: String::new(), ms: 0,
            has_error: true,
            error_msg: "Enter an English request first.".into(),
        }.render()?));
    }

    match rpc(&state, "v2/to.bund", json!({ "message": message })).await {
        Ok(v) => {
            let script         = string_field(&v, "script");
            let parse_error    = string_field(&v, "parse_error");
            let provider       = string_field(&v, "provider");
            let model          = string_field(&v, "model");
            let valid          = v.get("valid").and_then(|x| x.as_bool()).unwrap_or(false);
            let parse_attempts = v.get("parse_attempts").and_then(|x| x.as_u64()).unwrap_or(0);
            let ms             = v.get("ms").and_then(|x| x.as_u64()).unwrap_or(0);
            Ok(Html(BundTranslate {
                script, valid, parse_attempts, parse_error,
                provider, model, ms,
                has_error: false, error_msg: String::new(),
            }.render()?))
        }
        Err(AppError::Rpc(msg)) => Ok(Html(BundTranslate {
            script: String::new(), valid: false, parse_attempts: 0,
            parse_error: String::new(),
            provider: String::new(), model: String::new(), ms: 0,
            has_error: true,
            error_msg: msg,
        }.render()?)),
        Err(e) => Err(e),
    }
}

fn string_field(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_owned()
}
