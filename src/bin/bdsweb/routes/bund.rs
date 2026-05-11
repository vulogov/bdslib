use askama::Template;
use axum::{extract::State, response::Html, Form};
use serde::Deserialize;
use serde_json::json;

use crate::{client::{rpc, ModeBadge}, error::AppError, state::AppState};

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
                results, error_msg: String::new(), has_error: false,
                cluster_badge, cluster_meta_json,
            }.render()?))
        }
        Err(AppError::Rpc(msg)) => {
            Ok(Html(BundResult {
                results: vec![], error_msg: msg, has_error: true,
                cluster_badge: None, cluster_meta_json: String::new(),
            }.render()?))
        }
        Err(e) => Err(e),
    }
}
