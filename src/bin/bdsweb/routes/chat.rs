use askama::Template;
use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};

use crate::{admin::signed_rpc, client::{rpc, SESSION}, error::AppError, state::AppState};

const CHAT_COOKIE:    &str = "bds-chat-session";
const PROVIDER_COOKIE: &str = "bds-chat-provider";

// ── Provider picker plumbing ──────────────────────────────────────────────────
//
// On every page load we hit `v4/llm.providers.list` so the dropdown
// reflects what's actually registered on the node — operators can swap
// providers in `bds.hjson` without touching the UI.  An empty list is
// surfaced as a notice instead of an error so the chat page still
// renders even when no LLM providers have been configured yet.

#[derive(Clone, Debug)]
struct ProviderRow {
    id:            String,
    default_model: String,
    selected:      bool,
}

async fn load_providers(
    state:    &AppState,
    selected: Option<&str>,
) -> (Vec<ProviderRow>, Option<String>) {
    match signed_rpc(state, "v4/llm.providers.list", json!({})).await {
        Ok(v) => {
            let default = v.get("default")
                .and_then(|x| x.as_str())
                .map(str::to_owned);
            let chosen = selected.map(str::to_owned)
                .or_else(|| default.clone());
            let arr = v.get("providers").and_then(|x| x.as_array()).cloned()
                .unwrap_or_default();
            let rows: Vec<ProviderRow> = arr.into_iter().filter_map(|p| {
                let id    = p.get("id")?.as_str()?.to_owned();
                let model = p.get("default_model").and_then(|m| m.as_str())
                              .unwrap_or("").to_owned();
                let selected = chosen.as_deref() == Some(id.as_str());
                Some(ProviderRow { id, default_model: model, selected })
            }).collect();
            (rows, default)
        }
        Err(e) => {
            log::warn!("[chat] v4/llm.providers.list failed: {e}");
            (Vec::new(), None)
        }
    }
}

// ── Full page ─────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "chat.html")]
struct ChatPage {
    providers:    Vec<ProviderRow>,
    has_providers: bool,
    /// Empty when the operator hasn't configured cluster.shared_secret;
    /// v4/* requires HMAC so the chat page is read-only in that mode.
    needs_secret: bool,
}

pub async fn page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let preferred = extract_cookie(&headers, PROVIDER_COOKIE);
    let (providers, _default) = load_providers(&state, preferred.as_deref()).await;
    let has_providers = !providers.is_empty();
    let needs_secret = state.shared_secret.is_empty();
    Ok(Html(ChatPage { providers, has_providers, needs_secret }.render()?))
}

// ── Session reset ─────────────────────────────────────────────────────────────

pub async fn reset() -> Response {
    let mut resp = Redirect::to("/chat").into_response();
    // Reset the chat session id but KEEP the provider preference — the
    // operator picked a provider for a reason, no need to forget it
    // just because they wanted a fresh conversation.
    if let Ok(val) = HeaderValue::from_str(
        &format!("{CHAT_COOKIE}=; Path=/; Max-Age=0; SameSite=Strict; HttpOnly"),
    ) {
        resp.headers_mut().insert(header::SET_COOKIE, val);
    }
    resp
}

// ── Query POST ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct QueryForm {
    #[serde(default = "default_duration")]
    duration: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    provider: String,
}
fn default_duration() -> String { "1h".to_owned() }

const INIT_QUERY: &str = "A new analysis session has just started and telemetry key inventory has been \
    loaded as context. In 2–3 sentences, summarise: which keys or services appear most active \
    (highest record counts), any keys whose names suggest errors or anomalies, and what the \
    operator should investigate first. Be concise and direct — this is an opening briefing.";

#[derive(Template)]
#[template(path = "partials/chat_message.html")]
struct ChatMessage {
    user_query:       String,
    response:         String,
    error_msg:        String,
    has_error:        bool,
    has_context_note: bool,
    context_note:     String,
}

// ── New session POST ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct NewSessionForm {
    #[serde(default = "default_duration")]
    duration: String,
    #[serde(default)]
    provider: String,
}

pub async fn new_session(
    State(state): State<AppState>,
    Form(form): Form<NewSessionForm>,
) -> Result<Response, AppError> {
    // Step 1: fetch key inventory from bdsnode.  The exploration call
    // is unauthenticated (v2/* receiver) so it works in open-access
    // mode too — same as the legacy path.
    let explore_params = json!({
        "session":  SESSION,
        "duration": form.duration,
    });
    let explore_result = rpc(&state, "v2/primaries.explore", explore_params).await;

    let (context_str, n_keys) = match explore_result {
        Ok(ref arr) if arr.is_array() => {
            let items = arr.as_array().unwrap();
            let mut lines: Vec<String> = Vec::with_capacity(items.len());
            for item in items.iter().take(100) {
                let key   = item["key"].as_str().unwrap_or("?");
                let count = item["count"].as_u64().unwrap_or(0);
                lines.push(format!("  {key}: {count} records"));
            }
            let n = items.len();
            let body = if lines.is_empty() {
                "  (no telemetry data in the selected time window)".to_owned()
            } else {
                lines.join("\n")
            };
            (format!("Available telemetry keys (last {}):\n{}", form.duration, body), n)
        }
        _ => (
            format!("(telemetry key inventory unavailable for the last {})", form.duration),
            0,
        ),
    };

    // Step 2: open a new chat session via v4/llm.chat.  `chat_id: null`
    // signals "open a fresh session"; `provider` is optional and falls
    // back to the manager's default when empty.
    let provider_opt = if form.provider.is_empty() { None } else { Some(form.provider.clone()) };
    let chat_params = build_chat_params(
        &form.duration, INIT_QUERY, None, provider_opt.as_deref(),
        Some(context_str),
    );

    match signed_rpc(&state, "v4/llm.chat", chat_params).await {
        Ok(v) => {
            let chat_id  = v["chat_id"].as_str().unwrap_or("").to_owned();
            let response = v["response"].as_str().unwrap_or("").to_owned();
            let provider = v["provider"].as_str().unwrap_or("?").to_owned();
            let model    = v["model"].as_str().unwrap_or("?").to_owned();
            let cache    = v["cache"].as_str().unwrap_or("").to_owned();

            let context_note = format!(
                "New session started · {} key{} in the last {} · provider={provider} model={model}{}",
                n_keys,
                if n_keys == 1 { "" } else { "s" },
                form.duration,
                cache_suffix(&cache),
            );

            let html = ChatMessage {
                user_query:       String::new(),
                response,
                error_msg:        String::new(),
                has_error:        false,
                has_context_note: true,
                context_note,
            }.render()?;

            let mut resp = Html(html).into_response();
            set_cookie(&mut resp, CHAT_COOKIE, &chat_id);
            if !form.provider.is_empty() {
                set_cookie(&mut resp, PROVIDER_COOKIE, &form.provider);
            }
            Ok(resp)
        }
        Err(AppError::Rpc(msg)) => render_error(form.duration.as_str(), msg),
        Err(e) => Err(e),
    }
}

pub async fn query(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<QueryForm>,
) -> Result<Response, AppError> {
    if form.query.trim().is_empty() {
        return Ok((
            StatusCode::OK,
            Html(ChatMessage {
                user_query:       form.query,
                response:         String::new(),
                error_msg:        String::new(),
                has_error:        false,
                has_context_note: false,
                context_note:     String::new(),
            }.render()?),
        ).into_response());
    }

    let existing_id = extract_cookie(&headers, CHAT_COOKIE);
    let provider_opt = if form.provider.is_empty() { None } else { Some(form.provider.clone()) };

    let params = build_chat_params(
        &form.duration, &form.query, existing_id.as_deref(),
        provider_opt.as_deref(), None,
    );

    match signed_rpc(&state, "v4/llm.chat", params).await {
        Ok(v) => {
            let chat_id     = v["chat_id"].as_str().unwrap_or("").to_owned();
            let response    = v["response"].as_str().unwrap_or("").to_owned();
            let provider    = v["provider"].as_str().unwrap_or("?").to_owned();
            let model       = v["model"].as_str().unwrap_or("?").to_owned();
            let cache       = v["cache"].as_str().unwrap_or("").to_owned();
            let n_telemetry  = v["telemetry_count"].as_u64().unwrap_or(0);
            let n_docs       = v["document_count"].as_u64().unwrap_or(0);
            let prompt_chars = v["prompt_chars"].as_u64().unwrap_or(0);
            let num_ctx      = v["num_ctx"].as_u64();

            // Hard-flag the empty-RAG case — without context the model
            // is just answering the bare question and the operator
            // typically can't tell from the response alone.
            let context_note = if n_telemetry == 0 && n_docs == 0 {
                format!(
                    "⚠ NO RAG context loaded for last {} — model is answering without your data \
                     · provider={provider} model={model}{}",
                    form.duration, cache_suffix(&cache),
                )
            } else {
                let ctx_part = num_ctx.map(|n| format!(" · num_ctx={n}")).unwrap_or_default();
                format!(
                    "{} telemetry event{} + {} document{} · last {} · prompt={}ch{} · provider={provider} model={model}{}",
                    n_telemetry, if n_telemetry == 1 { "" } else { "s" },
                    n_docs,      if n_docs      == 1 { "" } else { "s" },
                    form.duration,
                    prompt_chars,
                    ctx_part,
                    cache_suffix(&cache),
                )
            };

            let html = ChatMessage {
                user_query:       form.query,
                response,
                error_msg:        String::new(),
                has_error:        false,
                has_context_note: true,
                context_note,
            }.render()?;

            let mut resp = Html(html).into_response();
            if existing_id.as_deref() != Some(&chat_id) {
                set_cookie(&mut resp, CHAT_COOKIE, &chat_id);
            }
            if !form.provider.is_empty() {
                set_cookie(&mut resp, PROVIDER_COOKIE, &form.provider);
            }
            Ok(resp)
        }
        Err(AppError::Rpc(msg)) => render_error_with_query(form.query, msg),
        Err(e) => Err(e),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn build_chat_params(
    duration: &str,
    message:  &str,
    chat_id:  Option<&str>,
    provider: Option<&str>,
    context:  Option<String>,
) -> JsonValue {
    let mut obj = serde_json::Map::new();
    obj.insert("session".into(),  json!(SESSION));
    obj.insert("chat_id".into(),  chat_id.map(|s| json!(s)).unwrap_or(JsonValue::Null));
    obj.insert("duration".into(), json!(duration));
    obj.insert("message".into(),  json!(message));
    if let Some(p) = provider { obj.insert("provider".into(), json!(p)); }
    if let Some(c) = context  { obj.insert("context".into(),  json!(c)); }
    JsonValue::Object(obj)
}

fn cache_suffix(cache: &str) -> String {
    if cache.is_empty() || cache == "miss" || cache.starts_with("disabled") {
        String::new()
    } else {
        format!(" · cache:{cache}")
    }
}

fn set_cookie(resp: &mut Response, name: &str, value: &str) {
    if value.is_empty() { return; }
    if let Ok(val) = HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; SameSite=Strict; HttpOnly"
    )) {
        resp.headers_mut().insert(header::SET_COOKIE, val);
    }
}

fn extract_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            let prefix = format!("{name}=");
            s.split(';')
             .map(|p| p.trim())
             .find(|p| p.starts_with(&prefix))
             .map(|p| p[prefix.len()..].to_string())
        })
        .filter(|s| !s.is_empty())
}

fn render_error(_duration: &str, msg: String) -> Result<Response, AppError> {
    let html = ChatMessage {
        user_query:       String::new(),
        response:         String::new(),
        error_msg:        msg,
        has_error:        true,
        has_context_note: false,
        context_note:     String::new(),
    }.render()?;
    Ok(Html(html).into_response())
}

fn render_error_with_query(query: String, msg: String) -> Result<Response, AppError> {
    let html = ChatMessage {
        user_query:       query,
        response:         String::new(),
        error_msg:        msg,
        has_error:        true,
        has_context_note: false,
        context_note:     String::new(),
    }.render()?;
    Ok(Html(html).into_response())
}
