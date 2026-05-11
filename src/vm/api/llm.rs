//! LLM helpers — sync-callable bridge between Bund / RPC handlers and the
//! async `bdslib::llm::providers::Provider` trait.
//!
//! Every helper:
//!
//! 1. Converts the `Value` input → JSON.
//! 2. Resolves the provider via [`crate::llm::manager::manager`].
//! 3. Drives the async provider method on the ambient or fallback
//!    tokio runtime via [`crate::vm::api::runtime::block_on`].
//! 4. Stashes a per-thread `llm_meta` block (provider, model, ms,
//!    tokens, cache state) on [`crate::vm::api::meta::set_llm`] so
//!    Bund scripts can read it via `?llm.meta`.
//!
//! Phase 1 ships `complete`, `embed`, `providers_list`.  `chat` lands
//! in its own step (needs the provider-agnostic chat-session module),
//! `analyze` waits for Phase 2 (context.rs).

use crate::common::jsonfingerprint::json_fingerprint;
use crate::llm::chat as llm_chat;
use crate::llm::context::{self as llm_ctx, ContextSource, RagContext};
use crate::llm::manager;
use crate::llm::providers::Provider;
use crate::llm::types::{
    CompletionOpts, CompletionRequest, CompletionResponse, EmbedRequest,
    EmbedResponse, Message, Role,
};
use crate::vm::api::{meta, runtime};
use crate::vm::helpers::eval::{dynamic_to_json, json_to_dynamic};
use easy_error::{bail, err_msg, Error};
use rust_dynamic::value::Value;
use serde_json::{json, Value as JsonValue};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

const DEFAULT_SYSTEM_PROMPT: &str =
    "You are an expert site reliability engineer and telemetry analyst with access to \
     real observability data. Analyse the provided context and answer the operator's \
     question concisely and accurately.";

// ─────────────────────────────────────────────────────────────────────
// Provider resolution + request construction
// ─────────────────────────────────────────────────────────────────────

fn resolve_provider(name: Option<&str>) -> Result<Arc<dyn Provider>, Error> {
    let mgr = manager::manager().ok_or_else(|| {
        err_msg("vm::api::llm: provider manager not initialised \
                 (bdslib::llm::manager::init was never called)")
    })?;
    mgr.resolve(name).map_err(|e| err_msg(format!("vm::api::llm: {e}")))
}

fn parse_messages(req: &JsonValue) -> Result<Vec<Message>, Error> {
    // Accept either {"prompt": "..."} (shortcut) or {"messages": [...]}.
    if let Some(arr) = req.get("messages").and_then(|v| v.as_array()) {
        let mut out = Vec::with_capacity(arr.len());
        for (i, m) in arr.iter().enumerate() {
            let role = m.get("role").and_then(|v| v.as_str())
                .ok_or_else(|| err_msg(format!("messages[{i}].role missing or not a string")))?;
            let content = m.get("content").and_then(|v| v.as_str())
                .ok_or_else(|| err_msg(format!("messages[{i}].content missing or not a string")))?;
            let role = match role {
                "system"    => Role::System,
                "user"      => Role::User,
                "assistant" => Role::Assistant,
                "tool"      => Role::Tool,
                other       => bail!("messages[{i}].role unknown variant: {other:?}"),
            };
            out.push(Message { role, content: content.to_owned() });
        }
        return Ok(out);
    }
    if let Some(p) = req.get("prompt").and_then(|v| v.as_str()) {
        return Ok(vec![Message::user(p)]);
    }
    bail!("vm::api::llm: request must include either `prompt` or `messages`")
}

fn parse_options(req: &JsonValue) -> CompletionOpts {
    let opts = match req.get("options").and_then(|v| v.as_object()) {
        Some(o) => o,
        None    => return CompletionOpts::default(),
    };
    CompletionOpts {
        temperature: opts.get("temperature").and_then(|v| v.as_f64()).map(|f| f as f32),
        max_tokens:  opts.get("max_tokens").and_then(|v| v.as_u64()).map(|n| n as u32),
        top_p:       opts.get("top_p").and_then(|v| v.as_f64()).map(|f| f as f32),
        stop:        opts.get("stop").and_then(|v| v.as_array())
                         .map(|arr| arr.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
                         .unwrap_or_default(),
        seed:        opts.get("seed").and_then(|v| v.as_u64()),
    }
}

fn req_as_object(req: Value, ctx: &str) -> Result<JsonValue, Error> {
    let json = dynamic_to_json(req);
    if !json.is_object() {
        bail!("{ctx}: request must be a Map / JSON object, got {}", short_kind(&json));
    }
    Ok(json)
}

fn short_kind(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null      => "null",
        JsonValue::Bool(_)   => "bool",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_)  => "array",
        JsonValue::Object(_) => "object",
    }
}

// ─────────────────────────────────────────────────────────────────────
// Public helpers
// ─────────────────────────────────────────────────────────────────────

/// `complete` — single-shot text completion.
///
/// Accepts a Value::Map with keys:
/// - `provider` (optional string) — name from the registry; default if absent
/// - `model`    (optional string) — provider-specific model id; provider's
///   default if absent
/// - `prompt`   (optional string) — shortcut for a single user message
/// - `messages` (optional list)   — explicit [{role, content}] turns
/// - `options`  (optional map)    — temperature, max_tokens, top_p, stop, seed
///
/// Returns a Map: `{response, provider, model, finish_reason?, tokens_in?,
/// tokens_out?, ms}`.
pub fn complete(req: Value) -> Result<Value, Error> {
    meta::clear_llm();
    let req_json = req_as_object(req, "vm::api::llm::complete")?;

    let provider_name = req_json.get("provider").and_then(|v| v.as_str());
    let provider = resolve_provider(provider_name)?;

    let model = req_json.get("model")
        .and_then(|v| v.as_str()).map(str::to_owned)
        .unwrap_or_else(|| provider.default_model().to_owned());

    let messages = parse_messages(&req_json)?;
    let options  = parse_options(&req_json);

    let rq = CompletionRequest { model: model.clone(), messages, options };

    let started = Instant::now();
    let resp: CompletionResponse = runtime::block_on(provider.complete(rq))
        .map_err(|e| err_msg(format!("vm::api::llm::complete: provider {:?}: {e}", provider.id())))?;
    let ms = started.elapsed().as_millis() as u64;

    let out = json!({
        "response":      resp.text,
        "provider":      provider.id(),
        "model":         resp.model,
        "finish_reason": resp.finish_reason,
        "tokens_in":     resp.tokens_in,
        "tokens_out":    resp.tokens_out,
        "ms":            ms,
    });
    meta::set_llm(json!({
        "provider":   provider.id(),
        "model":      model,
        "ms":         ms,
        "tokens_in":  out["tokens_in"].clone(),
        "tokens_out": out["tokens_out"].clone(),
        "cache":      "disabled",   // phase 3 flips this to hit/miss
    }));
    Ok(json_to_dynamic(out))
}

/// `embed` — produce vector embeddings for a list of texts.
///
/// Accepts a Value::Map with keys:
/// - `provider` (optional string)
/// - `model`    (optional string) — provider's default if absent
/// - `texts`    (list of strings) — OR
/// - `text`     (single string) — shortcut for `texts: [text]`
///
/// Returns a Map: `{vectors, dim, provider, model, ms}`.
pub fn embed(req: Value) -> Result<Value, Error> {
    meta::clear_llm();
    let req_json = req_as_object(req, "vm::api::llm::embed")?;

    let provider_name = req_json.get("provider").and_then(|v| v.as_str());
    let provider = resolve_provider(provider_name)?;

    if !provider.capabilities().embed {
        bail!("vm::api::llm::embed: provider {:?} does not support embeddings",
            provider.id());
    }

    let model = req_json.get("model")
        .and_then(|v| v.as_str()).map(str::to_owned)
        .unwrap_or_else(|| provider.default_model().to_owned());

    let texts: Vec<String> = if let Some(arr) = req_json.get("texts").and_then(|v| v.as_array()) {
        arr.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()
    } else if let Some(s) = req_json.get("text").and_then(|v| v.as_str()) {
        vec![s.to_owned()]
    } else {
        bail!("vm::api::llm::embed: request must include either `text` or `texts`");
    };
    if texts.is_empty() {
        bail!("vm::api::llm::embed: `texts` is empty");
    }

    let rq = EmbedRequest { model: model.clone(), texts };
    let started = Instant::now();
    let resp: EmbedResponse = runtime::block_on(provider.embed(rq))
        .map_err(|e| err_msg(format!("vm::api::llm::embed: provider {:?}: {e}", provider.id())))?;
    let ms = started.elapsed().as_millis() as u64;

    let out = json!({
        "vectors":  resp.vectors,
        "dim":      resp.dim,
        "provider": provider.id(),
        "model":    resp.model,
        "ms":       ms,
    });
    meta::set_llm(json!({
        "provider": provider.id(),
        "model":    model,
        "ms":       ms,
        "cache":    "disabled",
    }));
    Ok(json_to_dynamic(out))
}

/// `chat` — stateful chat turn (history persisted in the docstore).
///
/// Accepts a Value::Map with keys:
/// - `chat_id`       (optional string UUID) — when absent, a new session
///   is opened and `is_new_session: true` comes back
/// - `message`       (required string)      — the user's turn
/// - `provider`      (optional string)      — overrides session metadata for this turn
/// - `model`         (optional string)      — overrides session metadata for this turn
/// - `system_prompt` (optional string)      — only meaningful when opening
///   a new session; ignored on existing sessions
/// - `duration`      (optional string)      — when present and `context`
///   is absent, an inline RAG context is built via `db.aggregationsearch`
///   and prepended to the user message (matches the legacy
///   `v2/chat.ollama` behaviour)
/// - `context`       (optional string)      — pre-built RAG context that
///   replaces the inline aggregation pass
/// - `options`       (optional map)         — temperature, max_tokens, …
///
/// Returns `{chat_id, response, provider, model, is_new_session,
/// telemetry_count, document_count, finish_reason?, tokens_in?,
/// tokens_out?, ms}`.
pub fn chat(req: Value) -> Result<Value, Error> {
    meta::clear_llm();
    let req_json = req_as_object(req, "vm::api::llm::chat")?;

    let user_message = req_json.get("message").and_then(|v| v.as_str())
        .ok_or_else(|| err_msg("vm::api::llm::chat: `message` (string) is required"))?
        .to_owned();
    let provider_override = req_json.get("provider").and_then(|v| v.as_str()).map(str::to_owned);
    let model_override    = req_json.get("model").and_then(|v| v.as_str()).map(str::to_owned);
    let chat_id_str       = req_json.get("chat_id").and_then(|v| v.as_str()).map(str::to_owned);
    let system_prompt = req_json.get("system_prompt").and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned());
    let options = parse_options(&req_json);

    // Build RAG context: either supplied verbatim, or assembled from a
    // db.aggregationsearch over the requested duration.  Empty when
    // neither is present.
    let (rag_context, telemetry_count, document_count) = build_rag_context(&req_json)?;
    let enriched = if rag_context.is_empty() {
        user_message.clone()
    } else {
        let dur = req_json.get("duration").and_then(|v| v.as_str()).unwrap_or("recent window");
        format!(
            "Relevant observability context (last {dur}):\n\n{rag_context}\n\n---\n\nUser question: {user_message}"
        )
    };

    let outcome = match chat_id_str {
        Some(id_str) => {
            let chat_id = Uuid::parse_str(&id_str)
                .map_err(|e| err_msg(format!("vm::api::llm::chat: invalid chat_id {id_str:?}: {e}")))?;
            llm_chat::turn(
                chat_id,
                &enriched,
                provider_override.as_deref(),
                model_override.as_deref(),
                options,
            ).map_err(|e| err_msg(format!("vm::api::llm::chat: {e}")))?
        }
        None => llm_chat::open_and_turn(
            provider_override.as_deref(),
            model_override.as_deref(),
            &system_prompt,
            &enriched,
            options,
        ).map_err(|e| err_msg(format!("vm::api::llm::chat: {e}")))?,
    };

    let out = json!({
        "chat_id":         outcome.chat_id.to_string(),
        "response":        outcome.response,
        "provider":        outcome.provider,
        "model":           outcome.model,
        "is_new_session":  outcome.is_new_session,
        "telemetry_count": telemetry_count,
        "document_count":  document_count,
        "finish_reason":   outcome.finish_reason,
        "tokens_in":       outcome.tokens_in,
        "tokens_out":      outcome.tokens_out,
        "ms":              outcome.ms,
    });
    meta::set_llm(json!({
        "provider":   outcome.provider,
        "model":      outcome.model,
        "ms":         outcome.ms,
        "tokens_in":  outcome.tokens_in,
        "tokens_out": outcome.tokens_out,
        "cache":      "disabled",
    }));
    Ok(json_to_dynamic(out))
}

/// Resolve a RAG context string from `req`.  Priority:
/// 1. `context` (verbatim) → no DB hit
/// 2. `duration` + `query` (or `message` as fallback query) → run
///    `db.aggregationsearch` and fingerprint the top hits
/// 3. neither → empty
fn build_rag_context(req: &JsonValue) -> Result<(String, usize, usize), Error> {
    if let Some(c) = req.get("context").and_then(|v| v.as_str()) {
        return Ok((c.to_owned(), 0, 0));
    }
    let dur = match req.get("duration").and_then(|v| v.as_str()) {
        Some(d) => d,
        None    => return Ok((String::new(), 0, 0)),
    };
    let query = req.get("query").and_then(|v| v.as_str())
        .or_else(|| req.get("message").and_then(|v| v.as_str()))
        .unwrap_or("");
    let db = crate::globals::get_db()
        .map_err(|e| err_msg(format!("vm::api::llm::chat: get_db: {e}")))?;
    let agg = db.aggregationsearch(dur, query)
        .map_err(|e| err_msg(format!("vm::api::llm::chat: aggregationsearch: {e}")))?;
    let telemetry_hits: Vec<JsonValue> = agg.get("observability")
        .and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let doc_hits: Vec<JsonValue> = agg.get("documents")
        .and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let n_tel = telemetry_hits.len();
    let n_doc = doc_hits.len();

    let mut parts: Vec<String> = Vec::new();
    for (i, item) in telemetry_hits.iter().take(30).enumerate() {
        let fp = json_fingerprint(item);
        if !fp.is_empty() { parts.push(format!("[telemetry {}] {}", i + 1, fp)); }
    }
    for (i, item) in doc_hits.iter().take(10).enumerate() {
        let fp = json_fingerprint(item);
        if !fp.is_empty() { parts.push(format!("[document {}] {}", i + 1, fp)); }
    }
    Ok((parts.join("\n"), n_tel, n_doc))
}

/// `analyze` — build a RAG context from bdslib data and run a single
/// completion over it.
///
/// Request keys (rust_dynamic Map):
/// - `kind`            (required string)  — one of: aggregation, knn,
///   rca, anomaly, templates, telemetry, documents, supplied
/// - `duration`        (most kinds)       — humantime window
/// - `query`           (most kinds)       — the question / search text
/// - `prompt_template` (optional string)  — overrides the default per-kind preamble
/// - `provider`        (optional string)
/// - `model`           (optional string)
/// - `options`         (optional map)     — temperature / max_tokens / …
/// - `system_prompt`   (optional string)
///
/// Kind-specific extras:
/// - knn:        `k`
/// - rca:        `failure_key`, `bucket_secs`, `min_support`, `jaccard_threshold`, `max_keys`
/// - anomaly:    `limit`
/// - templates:  `top_n`
/// - telemetry:  `limit`
/// - documents:  `ids` (list of UUID strings)
/// - supplied:   `rows` (list of arbitrary JSON values)
///
/// Returns `{response, kind, source, provider, model, ms, finish_reason?,
/// tokens_in?, tokens_out?, n_rows}` where `source` is the underlying
/// `RagContext.source_meta` block.
pub fn analyze(req: Value) -> Result<Value, Error> {
    meta::clear_llm();
    let req_json = req_as_object(req, "vm::api::llm::analyze")?;

    let kind = req_json.get("kind").and_then(|v| v.as_str())
        .ok_or_else(|| err_msg("vm::api::llm::analyze: `kind` (string) is required"))?
        .to_owned();

    let source = build_context_source(&kind, &req_json)?;
    let rag: RagContext = llm_ctx::build(source)
        .map_err(|e| err_msg(format!("vm::api::llm::analyze: context: {e}")))?;

    let query = req_json.get("query").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let prompt_template = req_json.get("prompt_template").and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| default_prompt_for_kind(&kind).to_owned());
    let system_prompt = req_json.get("system_prompt").and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned());

    let user_message = compose_user_message(&kind, &rag, &prompt_template, &query);

    // Build a completion request and route through the same provider
    // resolution + meta-stashing as `complete`.
    let provider_name = req_json.get("provider").and_then(|v| v.as_str());
    let provider = resolve_provider(provider_name)?;
    let model = req_json.get("model").and_then(|v| v.as_str()).map(str::to_owned)
        .unwrap_or_else(|| provider.default_model().to_owned());

    let rq = CompletionRequest {
        model:    model.clone(),
        messages: vec![Message::system(&system_prompt), Message::user(&user_message)],
        options:  parse_options(&req_json),
    };

    let started = Instant::now();
    let resp: CompletionResponse = runtime::block_on(provider.complete(rq))
        .map_err(|e| err_msg(format!("vm::api::llm::analyze: provider {:?}: {e}", provider.id())))?;
    let ms = started.elapsed().as_millis() as u64;

    let out = json!({
        "response":      resp.text,
        "kind":          kind,
        "source":        rag.source_meta,
        "n_rows":        rag.n_rows,
        "provider":      provider.id(),
        "model":         resp.model,
        "finish_reason": resp.finish_reason,
        "tokens_in":     resp.tokens_in,
        "tokens_out":    resp.tokens_out,
        "ms":            ms,
    });
    meta::set_llm(json!({
        "provider":   provider.id(),
        "model":      model,
        "ms":         ms,
        "tokens_in":  out["tokens_in"].clone(),
        "tokens_out": out["tokens_out"].clone(),
        "kind":       kind,
        "n_rows":     rag.n_rows,
        "cache":      "disabled",
    }));
    Ok(json_to_dynamic(out))
}

fn build_context_source(kind: &str, req: &JsonValue) -> Result<ContextSource, Error> {
    let dur = |field: &str| req.get(field).and_then(|v| v.as_str()).map(str::to_owned);
    let query_field = || req.get("query").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let limit_field = || req.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize);
    let require_duration = || dur("duration").ok_or_else(||
        err_msg(format!("vm::api::llm::analyze: kind={kind:?} requires `duration`")));

    match kind {
        "aggregation" => Ok(ContextSource::Aggregation {
            duration: require_duration()?,
            query:    query_field(),
            limit:    limit_field(),
        }),
        "knn" => Ok(ContextSource::Knn {
            duration: require_duration()?,
            query:    query_field(),
            k:        req.get("k").and_then(|v| v.as_u64()).map(|n| n as usize),
        }),
        "rca" => Ok(ContextSource::Rca {
            duration:          require_duration()?,
            failure_key:       req.get("failure_key").and_then(|v| v.as_str()).map(str::to_owned),
            bucket_secs:       req.get("bucket_secs").and_then(|v| v.as_u64()),
            min_support:       req.get("min_support").and_then(|v| v.as_u64()),
            jaccard_threshold: req.get("jaccard_threshold").and_then(|v| v.as_f64()).map(|f| f as f32),
            max_keys:          req.get("max_keys").and_then(|v| v.as_u64()).map(|n| n as usize),
        }),
        "anomaly" => Ok(ContextSource::Anomaly {
            duration: require_duration()?,
            limit:    limit_field(),
        }),
        "templates" => Ok(ContextSource::Templates {
            duration: require_duration()?,
            top_n:    req.get("top_n").and_then(|v| v.as_u64()).map(|n| n as usize),
        }),
        "telemetry" => Ok(ContextSource::Telemetry {
            duration: require_duration()?,
            query:    req.get("query").and_then(|v| v.as_str()).map(str::to_owned),
            limit:    limit_field(),
        }),
        "documents" => {
            let arr = req.get("ids").and_then(|v| v.as_array())
                .ok_or_else(|| err_msg("vm::api::llm::analyze: kind=documents requires `ids` array"))?;
            let mut ids = Vec::with_capacity(arr.len());
            for (i, v) in arr.iter().enumerate() {
                let s = v.as_str().ok_or_else(||
                    err_msg(format!("vm::api::llm::analyze: ids[{i}] not a string")))?;
                let u = Uuid::parse_str(s).map_err(|e|
                    err_msg(format!("vm::api::llm::analyze: ids[{i}]={s:?}: {e}")))?;
                ids.push(u);
            }
            Ok(ContextSource::Documents { ids })
        }
        "supplied" => {
            let arr = req.get("rows").and_then(|v| v.as_array())
                .ok_or_else(|| err_msg("vm::api::llm::analyze: kind=supplied requires `rows` array"))?
                .clone();
            Ok(ContextSource::Supplied { rows: arr })
        }
        other => bail!("vm::api::llm::analyze: unknown kind {other:?} (expected one of: \
            aggregation, knn, rca, anomaly, templates, telemetry, documents, supplied)"),
    }
}

fn default_prompt_for_kind(kind: &str) -> &'static str {
    match kind {
        "aggregation" => "Summarize the key findings in the following observability data.",
        "knn"         => "Identify recurring patterns and outliers in these k-nearest neighbours.",
        "rca"         => "Explain the most likely root cause indicated by these candidate clusters.",
        "anomaly"     => "Identify and explain the anomalies represented by these candidates.",
        "templates"   => "Summarize the dominant log templates and what they suggest about system behaviour.",
        "telemetry"   => "Summarize the recent telemetry and call out anything noteworthy.",
        "documents"   => "Summarize the key information across these documents.",
        "supplied"    => "Analyze the following supplied rows.",
        _             => "Analyze the following data.",
    }
}

/// Compose the user-message turn that gets handed to the provider.
/// Empty context (e.g. supplied with no rows) still goes through —
/// the model just gets the question without a preamble.
fn compose_user_message(kind: &str, rag: &RagContext, prompt_template: &str, query: &str) -> String {
    let mut buf = String::new();
    buf.push_str(prompt_template);
    if !rag.summary.is_empty() {
        if !buf.is_empty() { buf.push_str("\n\n"); }
        buf.push_str(&format!("Relevant {kind} context ({n} row(s)):\n",
            n = rag.n_rows));
        buf.push_str(&rag.summary);
    }
    if !query.is_empty() {
        if !buf.is_empty() { buf.push_str("\n\n---\n\n"); }
        buf.push_str("Question: ");
        buf.push_str(query);
    }
    buf
}

/// `providers_list` — registered providers + the default.
///
/// Returns `{default, providers: [{id, default_model, capabilities: {chat, embed}}, …]}`
/// or `{default: null, providers: []}` when the manager is unset / empty.
pub fn providers_list() -> Result<Value, Error> {
    meta::clear_llm();
    let mgr = match manager::manager() {
        Some(m) => m,
        None    => {
            return Ok(json_to_dynamic(json!({ "default": null, "providers": [] })));
        }
    };
    let names = mgr.registered();
    let mut providers: Vec<JsonValue> = Vec::with_capacity(names.len());
    for n in &names {
        if let Ok(p) = mgr.get(n) {
            let c = p.capabilities();
            providers.push(json!({
                "id":            p.id(),
                "default_model": p.default_model(),
                "capabilities":  { "chat": c.chat, "embed": c.embed },
            }));
        }
    }
    Ok(json_to_dynamic(json!({
        "default":   mgr.default_id(),
        "providers": providers,
    })))
}
