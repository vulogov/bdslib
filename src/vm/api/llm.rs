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
use crate::llm::cache::{self as cache, CacheInsert, CacheManager, CachedEntry};
use crate::llm::chat as llm_chat;
use crate::llm::chat_bund;
use crate::llm::context::{self as llm_ctx, ContextSource, RagContext};
use crate::llm::dedup::{self, InferenceLease, InferenceState};
use crate::llm::jobs::{self, JobInsert, JobState, ListFilter as JobListFilter};
use crate::llm::manager;
use crate::llm::providers::Provider;
use crate::llm::snippet;
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::thread;
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
        num_ctx:     opts.get("num_ctx").and_then(|v| v.as_u64()).map(|n| n as u32),
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
// Cache disposition
// ─────────────────────────────────────────────────────────────────────

/// Decision the cache layer made for one helper invocation.
///
/// `Enabled(mgr)` means we'll do a get-by-key + a put on miss.
/// `Disabled(reason)` shapes the `cache` field in the response and llm
/// meta so callers can see *why* the cache was bypassed.  Reasons:
///   - `"disabled"`            → global config or no cache manager
///   - `"disabled:opt-out"`    → per-call `cache: false`
///   - `"disabled:temperature"`→ temperature > 0 (non-deterministic
///     workflows shouldn't be cached — see Risk #2 in the proposal)
enum CacheDisposition {
    Enabled(&'static CacheManager),
    Disabled(&'static str),
}

impl CacheDisposition {
    fn miss_label(&self) -> &'static str {
        match self {
            Self::Enabled(_)     => "miss",
            Self::Disabled(r)    => r,
        }
    }
}

fn cache_disposition(req: &JsonValue, opts: &CompletionOpts) -> CacheDisposition {
    if req.get("cache").and_then(|v| v.as_bool()) == Some(false) {
        return CacheDisposition::Disabled("disabled:opt-out");
    }
    let mgr = match cache::manager() {
        Some(m) if m.enabled() => m,
        _                      => return CacheDisposition::Disabled("disabled"),
    };
    // Don't cache non-deterministic completions.  `temperature == 0`
    // and `unset` both count as deterministic (most providers default
    // to a low value that still round-trips identically on rerun for
    // a given seed; we conservatively cache only when the operator
    // hasn't explicitly dialled in randomness).
    if let Some(t) = opts.temperature {
        if t > 0.0 {
            return CacheDisposition::Disabled("disabled:temperature");
        }
    }
    CacheDisposition::Enabled(mgr)
}

fn messages_to_canonical(msgs: &[Message]) -> JsonValue {
    JsonValue::Array(msgs.iter()
        .map(|m| json!({"role": m.role.as_str(), "content": m.content}))
        .collect())
}

fn options_to_canonical(opts: &CompletionOpts) -> JsonValue {
    let mut obj = serde_json::Map::new();
    if let Some(t) = opts.temperature { obj.insert("temperature".into(), json!(t)); }
    if let Some(m) = opts.max_tokens  { obj.insert("max_tokens".into(),  json!(m)); }
    if let Some(p) = opts.top_p       { obj.insert("top_p".into(),       json!(p)); }
    if let Some(s) = opts.seed        { obj.insert("seed".into(),        json!(s)); }
    if !opts.stop.is_empty()          { obj.insert("stop".into(),        json!(opts.stop)); }
    JsonValue::Object(obj)
}

fn response_to_cache(resp: &CompletionResponse) -> JsonValue {
    json!({
        "text":          resp.text,
        "model":         resp.model,
        "finish_reason": resp.finish_reason,
        "tokens_in":     resp.tokens_in,
        "tokens_out":    resp.tokens_out,
    })
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Try a cache lookup for `key`.  Returns `Some((entry, age_ms))` on
/// hit (and bumps the `hits` counter); `None` on miss / disabled /
/// errors (errors are logged so a broken cache never blocks the user).
fn cache_lookup(disposition: &CacheDisposition, key: &str) -> Option<CachedEntry> {
    let mgr = match disposition {
        CacheDisposition::Enabled(m) => *m,
        CacheDisposition::Disabled(_) => return None,
    };
    match mgr.cache().get_by_key(key) {
        Ok(Some(entry)) => {
            if let Err(e) = mgr.cache().bump_hits(entry.id) {
                log::debug!("vm::api::llm: bump_hits failed: {e}");
            }
            Some(entry)
        }
        Ok(None) => None,
        Err(e) => {
            log::debug!("vm::api::llm: cache.get_by_key failed: {e}");
            None
        }
    }
}

/// Best-effort cache write.  Failures are logged; the user already
/// has their response.  In cluster mode the same row is fan-out
/// replicated to every Alive peer via `v2/llm.cache.put` so peer
/// caches converge without waiting on anti-entropy.
fn cache_store(
    disposition: &CacheDisposition,
    cache_key:   &str,
    provider:    &str,
    model:       &str,
    kind:        &str,
    canonical:   JsonValue,
    response:    JsonValue,
    source_meta: Option<JsonValue>,
) {
    let mgr = match disposition {
        CacheDisposition::Enabled(m) => *m,
        CacheDisposition::Disabled(_) => return,
    };
    let insert = CacheInsert {
        id:            Uuid::now_v7(),
        cache_key:     cache_key.to_owned(),
        provider:      provider.to_owned(),
        model:         model.to_owned(),
        kind:          kind.to_owned(),
        request_json:  canonical.clone(),
        response_json: response.clone(),
        source_meta:   source_meta.clone(),
        created_at:    now_secs(),
        expires_at:    mgr.expires_at_for_now(),
    };
    let id = insert.id;
    let created_at = insert.created_at;
    let expires_at = insert.expires_at;
    if let Err(e) = mgr.cache().put(insert) {
        log::debug!("vm::api::llm: cache.put failed: {e}");
        return;
    }
    // Cluster replication: best-effort fan-out to every Alive peer.
    // We don't fail the user's request when replication fails — the
    // entry already exists locally and anti-entropy will catch up.
    if let Ok(db) = crate::globals::get_db() {
        if let Some(cluster) = db.cluster() {
            let cluster = cluster.clone();
            let params = json!({
                "id":            id.to_string(),
                "cache_key":     cache_key,
                "provider":      provider,
                "model":         model,
                "kind":          kind,
                "request_json":  canonical,
                "response_json": response,
                "source_meta":   source_meta,
                "created_at":    created_at,
                "expires_at":    expires_at,
            });
            let _ = runtime::block_on(
                crate::cluster::replication::replicate_to_all(cluster, "v2/llm.cache.put", params),
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Dedup — cluster-wide single-execution lease
// ─────────────────────────────────────────────────────────────────────

/// Decision the dedup layer made for one helper invocation.
enum LeaseOutcome {
    /// We're going to run the inference; the lease must be released.
    Acquired(InferenceLease),
    /// Another node is currently running the same `cache_key` (or we
    /// are, on a different thread).  Caller polls the cache for
    /// `wait_max_secs` before falling through to run anyway.
    SkipRunning,
    /// A peer finished recently — the cache should already have it.
    /// Caller does a cache re-lookup; on miss, falls through.
    SkipDone,
    /// Dedup is disabled, no cluster, or no DB — run unconditionally
    /// without a lease.
    Disabled,
}

impl LeaseOutcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Acquired(_)  => "ran",
            Self::SkipRunning  => "waited",
            Self::SkipDone     => "skipped:done",
            Self::Disabled     => "disabled",
        }
    }
}

/// Look up local + peer inference logs and either mint a `running` row
/// or report what's already in-flight / recently done.
fn try_acquire_lease(cache_key: &str) -> LeaseOutcome {
    let settings = dedup::settings();
    if !settings.enabled {
        return LeaseOutcome::Disabled;
    }
    let db = match crate::globals::get_db() {
        Ok(d)  => d,
        Err(_) => return LeaseOutcome::Disabled,
    };
    let cluster = match db.cluster() {
        Some(c) => c.clone(),
        None    => return LeaseOutcome::Disabled,  // standalone — nothing to dedup against
    };
    let log = cluster.inference_log.clone();

    // 1. Local check first — avoids a cluster round-trip when this
    //    same node is already running the work.
    if let Ok(Some(row)) = log.recent_within(cache_key, settings.window_secs) {
        match row.state {
            InferenceState::Running => return LeaseOutcome::SkipRunning,
            InferenceState::Done    => return LeaseOutcome::SkipDone,
            InferenceState::Failed  => { /* retry */ }
        }
    }

    // 2. Cluster check — fan v2/llm.last_executed out to every Alive peer.
    let fan = runtime::block_on(crate::cluster::fanout::fan_out_v2(
        &cluster, "v2/llm.last_executed",
        json!({ "cache_key": cache_key, "window_secs": settings.window_secs }),
    ));
    let now = dedup::now_secs();
    for body in fan.ok_results() {
        if body.get("found").and_then(|v| v.as_bool()) != Some(true) { continue; }
        let started_at = body.get("started_at").and_then(|v| v.as_u64()).unwrap_or(0);
        // Apply the local window in case the peer didn't.
        if settings.window_secs > 0
            && now.saturating_sub(started_at) > settings.window_secs {
            continue;
        }
        match body.get("state").and_then(|v| v.as_str()) {
            Some("running") => return LeaseOutcome::SkipRunning,
            Some("done")    => return LeaseOutcome::SkipDone,
            _               => {}
        }
    }

    // 3. Acquire — record a fresh `running` row locally.  See the
    //    scheduler_log race-window note: two nodes can pass step 2
    //    in the same sub-second window and both record a running
    //    row; the cache prevents the second one from doing real work.
    if let Err(e) = log.record_start(cache_key, cluster.node_id, dedup::now_secs()) {
        log::warn!("[llm::dedup] record_start failed: {e}");
        return LeaseOutcome::Disabled;
    }
    LeaseOutcome::Acquired(InferenceLease::new(
        log, cache_key.to_owned(), cluster.node_id,
    ))
}

/// Poll the inference cache for up to `wait_max_secs`, looking for the
/// entry that a peer is presumably about to write.  Returns `Some` on
/// arrival, `None` after the deadline expires.
fn wait_for_cache_entry(
    disposition: &CacheDisposition,
    cache_key:   &str,
) -> Option<CachedEntry> {
    let settings = dedup::settings();
    if settings.wait_max_secs == 0 {
        return None;  // "fail fast" mode
    }
    let mgr = match disposition {
        CacheDisposition::Enabled(m)  => *m,
        CacheDisposition::Disabled(_) => return None,
    };
    let deadline = Instant::now() + Duration::from_secs(settings.wait_max_secs);
    while Instant::now() < deadline {
        match mgr.cache().get_by_key(cache_key) {
            Ok(Some(entry)) => {
                if let Err(e) = mgr.cache().bump_hits(entry.id) {
                    log::debug!("vm::api::llm: bump_hits after wait failed: {e}");
                }
                return Some(entry);
            }
            Ok(None) => {}
            Err(e) => {
                log::debug!("vm::api::llm: wait_for_cache_entry cache.get failed: {e}");
                return None;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    None
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

    let disposition = cache_disposition(&req_json, &options);
    let canonical = json!({
        "kind":     "complete",
        "provider": provider.id(),
        "model":    model.clone(),
        "messages": messages_to_canonical(&messages),
        "options":  options_to_canonical(&options),
    });
    let key = cache::cache_key(&canonical);

    // Cache hit short-circuit — no provider call.
    if let Some(entry) = cache_lookup(&disposition, &key) {
        return Ok(json_to_dynamic(cache_hit_response(&entry, "complete", None, None)));
    }

    // Phase 4 — dedup gate.  On SkipRunning / SkipDone we poll the
    // cache briefly hoping the peer's replicated result arrives, then
    // fall through and run the inference unconditionally (sync caller
    // doesn't want to wait forever).
    let lease_outcome = try_acquire_lease(&key);
    if matches!(lease_outcome, LeaseOutcome::SkipRunning | LeaseOutcome::SkipDone) {
        if let Some(entry) = wait_for_cache_entry(&disposition, &key) {
            return Ok(json_to_dynamic(cache_hit_response(
                &entry, "complete", Some(lease_outcome.label()), None,
            )));
        }
    }
    let dedup_label = lease_outcome.label();

    let rq = CompletionRequest { model: model.clone(), messages, options };
    let started = Instant::now();
    let resp_result = runtime::block_on(provider.complete(rq));
    let ms = started.elapsed().as_millis() as u64;

    // Release the lease (if we hold one) before returning.  Drop'd
    // leases mark `failed` automatically, but explicit release sets
    // the right state on both paths.
    let resp: CompletionResponse = match (resp_result, lease_outcome) {
        (Ok(r), LeaseOutcome::Acquired(lease)) => { lease.release_done();   r }
        (Err(e), LeaseOutcome::Acquired(lease)) => { lease.release_failed();
            return Err(err_msg(format!("vm::api::llm::complete: provider {:?}: {e}",
                provider.id()))); }
        (Ok(r),  _) => r,
        (Err(e), _) => return Err(err_msg(format!(
            "vm::api::llm::complete: provider {:?}: {e}", provider.id()))),
    };

    let miss_label = disposition.miss_label();
    cache_store(&disposition, &key, provider.id(), &resp.model, "complete",
                canonical, response_to_cache(&resp), None);

    let out = json!({
        "response":      resp.text,
        "provider":      provider.id(),
        "model":         resp.model,
        "finish_reason": resp.finish_reason,
        "tokens_in":     resp.tokens_in,
        "tokens_out":    resp.tokens_out,
        "ms":            ms,
        "cache":         miss_label,
        "dedup":         dedup_label,
    });
    meta::set_llm(json!({
        "provider":   provider.id(),
        "model":      model,
        "ms":         ms,
        "tokens_in":  out["tokens_in"].clone(),
        "tokens_out": out["tokens_out"].clone(),
        "cache":      miss_label,
        "dedup":      dedup_label,
    }));
    Ok(json_to_dynamic(out))
}

/// Build the response Map for a cache hit.  Centralised so `complete`
/// and `analyze` produce the same shape on hit + on dedup-wait-success.
/// `dedup` is the LeaseOutcome label when this hit was the result of
/// waiting on a peer; `None` for ordinary cache hits.
fn cache_hit_response(
    entry:        &CachedEntry,
    kind_label:   &str,
    dedup_label:  Option<&'static str>,
    extra_source: Option<JsonValue>,
) -> JsonValue {
    let mut out = json!({
        "response":      entry.response_json.get("text").and_then(|v| v.as_str())
                              .unwrap_or("").to_owned(),
        "provider":      entry.provider,
        "model":         entry.model,
        "finish_reason": entry.response_json.get("finish_reason").cloned()
                              .unwrap_or(JsonValue::Null),
        "tokens_in":     entry.response_json.get("tokens_in").cloned()
                              .unwrap_or(JsonValue::Null),
        "tokens_out":    entry.response_json.get("tokens_out").cloned()
                              .unwrap_or(JsonValue::Null),
        "ms":            0,
        "cache":         "hit",
    });
    if let Some(d) = dedup_label {
        out["dedup"] = json!(d);
    }
    if let Some(src) = extra_source {
        out["source"] = src;
    }
    meta::set_llm(json!({
        "provider":   out["provider"].clone(),
        "model":      out["model"].clone(),
        "ms":         0,
        "tokens_in":  out["tokens_in"].clone(),
        "tokens_out": out["tokens_out"].clone(),
        "cache":      "hit",
        "dedup":      dedup_label.unwrap_or("disabled"),
    }));
    let _ = kind_label;  // reserved for future use (per-kind hit telemetry)
    out
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
    let mut options = parse_options(&req_json);

    // ── Bund snippet detection ────────────────────────────────────
    // Pure parser, no eval yet.  Snippet detection happens first so a
    // snippet-bearing message bypasses the RAG aggregationsearch
    // entirely.  See Documentation/LLM.md § _Bund chat snippets_.
    let bund_settings = chat_bund::settings();
    let snippet = snippet::extract_bund_snippet(&user_message, snippet::DetectOpts {
        fenced_only:      bund_settings.fenced_only,
        slash_strictness: bund_settings.slash_strictness,
    });
    // Per-call override — request can force-disable a globally-enabled
    // feature but NOT force-enable a globally-disabled one (that would
    // be a privilege escalation).
    let bund_enabled_call = req_json.get("bund_enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let run_snippet = snippet.is_some() && bund_settings.enabled && bund_enabled_call;

    if snippet.is_some() && !run_snippet {
        log::info!(
            "[llm::chat] bund snippet detected but skipped \
             (chat.bund.enabled={}, per_call_override={}) — falling back to RAG \
             with the literal message as the search query",
            bund_settings.enabled, bund_enabled_call
        );
    }

    // ── Bund snippet eval branch ──────────────────────────────────
    if run_snippet {
        let snip = snippet.as_ref().unwrap();
        let timeout = std::time::Duration::from_secs(bund_settings.timeout_secs);
        log::info!(
            "[llm::chat] bund eval start (source={} code_chars={} timeout={}s)",
            snip.source.as_str(), snip.code.len(), bund_settings.timeout_secs
        );

        match chat_bund::eval_snippet(snip.code.clone(), timeout) {
            Ok(success) => {
                return run_chat_with_bund_result(
                    &req_json, &user_message,
                    chat_id_str.as_deref(),
                    provider_override.as_deref(),
                    model_override.as_deref(),
                    &system_prompt,
                    options,
                    snip,
                    success,
                    bund_settings,
                );
            }
            Err(err) => {
                log::warn!(
                    "[llm::chat] bund eval failed ({}): {} — aborting LLM call",
                    err.kind(), err.message()
                );
                return Ok(json_to_dynamic(early_return_bund_error(
                    chat_id_str.as_deref(), snip, err, bund_settings,
                )));
            }
        }
    }

    // Resolve the chat_id path BEFORE building the enriched prompt so
    // we can mine prior Bund-snippet pins from history.  Same
    // cookie-recovery semantics as below: a stale chat_id silently
    // opens a new session.
    let resume_id = match chat_id_str.as_deref() {
        Some(id_str) => {
            let chat_id = Uuid::parse_str(id_str)
                .map_err(|e| err_msg(format!("vm::api::llm::chat: invalid chat_id {id_str:?}: {e}")))?;
            match llm_chat::session_metadata(chat_id) {
                Ok(Some(_)) => Some(chat_id),
                Ok(None)    => {
                    log::info!("vm::api::llm::chat: chat_id {chat_id} not found in docstore — \
                                opening a new session");
                    None
                }
                Err(e) => {
                    log::warn!("vm::api::llm::chat: session_metadata({chat_id}) failed: {e} \
                                — falling back to new session");
                    None
                }
            }
        }
        None => None,
    };

    // Bund-snippet pin: when the chat has prior snippet runs, surface
    // up to 2 of the most recent ones (truncated to 1024 chars each)
    // as a high-priority preamble to the user message.  Without this,
    // a 50k-char doc RAG dump on the follow-up turn buries the prior
    // snippet result in user-1, and the model loses track of what it
    // just told the operator.
    let bund_pins: Vec<crate::llm::chat::BundPin> = resume_id
        .and_then(|cid| llm_chat::recent_bund_pins(cid, 2, 1024).ok())
        .unwrap_or_default();
    let pin_block = if bund_pins.is_empty() {
        String::new()
    } else {
        let mut s = String::from(
            "Earlier in this conversation, the operator ran Bund snippets — \
             these results are authoritative for any follow-up question:\n\n");
        for (i, p) in bund_pins.iter().enumerate() {
            s.push_str(&format!(
                "[snippet {}]\n```bund\n{}\n```\n→ {}\n",
                i + 1, p.code, p.result
            ));
            if !p.summary.is_empty() {
                s.push_str(&format!("(model said: {})\n", p.summary));
            }
            s.push('\n');
        }
        s.push_str("---\n\n");
        s
    };

    // Build RAG context: either supplied verbatim, or assembled from a
    // db.aggregationsearch over the requested duration.  Empty when
    // neither is present.
    let (rag_context, telemetry_count, document_count) = build_rag_context(&req_json)?;
    let enriched = if rag_context.is_empty() && pin_block.is_empty() {
        user_message.clone()
    } else if rag_context.is_empty() {
        format!("{pin_block}User question: {user_message}")
    } else {
        let dur = req_json.get("duration").and_then(|v| v.as_str()).unwrap_or("recent window");
        format!(
            "{pin_block}Relevant observability context (last {dur}):\n\n{rag_context}\n\n---\n\nUser question: {user_message}"
        )
    };

    // ── Low-RAG suggestion ───────────────────────────────────────
    // When the operator asked for a `duration`-windowed RAG but both
    // counts came back zero, surface a `suggest_bund` block with
    // canned snippets they could try instead.  Pure keyword
    // heuristics; no LLM call required.
    let suggest_bund_snippets: Vec<crate::llm::chat_bund::Suggestion> =
        if telemetry_count == 0
            && document_count == 0
            && req_json.get("duration").is_some()
            && !run_snippet
        {
            chat_bund::suggest_for_query(&user_message)
        } else {
            Vec::new()
        };

    // Ollama's default num_ctx is 2048 tokens; non-trivial RAG blows
    // past that and the runtime silently truncates from the start of
    // the prompt, so the retrieved rows never reach the model.  Pick
    // a generous context window based on the actual prompt size when
    // the caller didn't override it.  4 chars/token is a conservative
    // estimate; we round up to a power-of-two-ish bucket Ollama
    // handles cleanly (8k / 16k / 32k / 64k).  Non-Ollama providers
    // ignore num_ctx, so this is free on OpenAI / Anthropic.
    if options.num_ctx.is_none() {
        let approx_tokens = (enriched.len() + system_prompt.len()) / 4;
        options.num_ctx = Some(match approx_tokens {
            0..=4096       => 8192,
            4097..=8192    => 16384,
            8193..=16384   => 32768,
            _              => 65536,
        });
    }
    let prompt_chars = enriched.len();
    let chosen_num_ctx = options.num_ctx;

    let outcome = match resume_id {
        Some(chat_id) => llm_chat::turn(
            chat_id,
            &enriched,
            provider_override.as_deref(),
            model_override.as_deref(),
            options,
        ).map_err(|e| err_msg(format!("vm::api::llm::chat: {e}")))?,
        None => llm_chat::open_and_turn(
            provider_override.as_deref(),
            model_override.as_deref(),
            &system_prompt,
            &enriched,
            options,
        ).map_err(|e| err_msg(format!("vm::api::llm::chat: {e}")))?,
    };

    // Chat turns are NOT cached: every turn produces NEW history
    // and the response depends on the running context.  A cache layer
    // for chat would have to key on the full history snapshot, which
    // never repeats — so we expose `cache: "disabled:chat"` to make
    // the absence explicit.
    let cache_label = "disabled:chat";

    let mut out = json!({
        "chat_id":         outcome.chat_id.to_string(),
        "response":        outcome.response,
        "provider":        outcome.provider,
        "model":           outcome.model,
        "is_new_session":  outcome.is_new_session,
        "telemetry_count": telemetry_count,
        "document_count":  document_count,
        "prompt_chars":    prompt_chars,
        "num_ctx":         chosen_num_ctx,
        "finish_reason":   outcome.finish_reason,
        "tokens_in":       outcome.tokens_in,
        "tokens_out":      outcome.tokens_out,
        "ms":              outcome.ms,
        "cache":           cache_label,
    });
    if !suggest_bund_snippets.is_empty() {
        out["suggest_bund"] = JsonValue::Array(
            suggest_bund_snippets.iter().map(|s| s.to_json()).collect()
        );
    }
    log::info!(
        "[llm::chat] sent prompt to provider={} model={} prompt_chars={} num_ctx={:?} \
         (telemetry={telemetry_count} docs={document_count} tokens_in={:?} tokens_out={:?})",
        outcome.provider, outcome.model, prompt_chars, chosen_num_ctx,
        outcome.tokens_in, outcome.tokens_out,
    );
    meta::set_llm(json!({
        "provider":     outcome.provider,
        "model":        outcome.model,
        "ms":           outcome.ms,
        "tokens_in":    outcome.tokens_in,
        "tokens_out":   outcome.tokens_out,
        "prompt_chars": prompt_chars,
        "num_ctx":      chosen_num_ctx,
        "cache":        cache_label,
    }));
    Ok(json_to_dynamic(out))
}

/// Resolve a RAG context string from `req`.  Priority:
/// 1. `context` (verbatim) → no DB hit
/// 2. `duration` + `query` (or `message` as fallback query) → run
///    the cluster-aware `vm::api::search::aggregation_search` (which
///    fans out to v2/aggregationsearch on every Alive peer and merges
///    via `cluster::merge::dedup_avg_score`).  Fingerprints the top
///    hits and joins them into a prompt-ready context string.
// ─────────────────────────────────────────────────────────────────────
// Bund snippet helpers — § _Bund chat snippets_ in LLM.md
// ─────────────────────────────────────────────────────────────────────

/// Snippet succeeded; splice JSON result into the prompt, call LLM,
/// build response with full bund stats block.  Returns the response
/// `Value` ready for `json_to_dynamic`.
#[allow(clippy::too_many_arguments)]
fn run_chat_with_bund_result(
    req_json:           &JsonValue,
    user_message:       &str,
    chat_id_str:        Option<&str>,
    provider_override:  Option<&str>,
    model_override:     Option<&str>,
    system_prompt:      &str,
    mut options:        CompletionOpts,
    snip:               &snippet::BundSnippet,
    success:            chat_bund::BundEvalSuccess,
    bund_settings:      &chat_bund::ChatBundSettings,
) -> Result<Value, Error> {
    // Format the workbench result for the prompt.
    let formatted = match chat_bund::format_for_prompt(
        &success.items,
        bund_settings.max_result_chars,
        bund_settings.oversize_strategy,
    ) {
        Ok(f) => f,
        Err(msg) => {
            // Drop strategy hit the size cap — bubble as a side-channel
            // error, no LLM call.
            return Ok(json_to_dynamic(early_return_bund_error(
                chat_id_str, snip,
                chat_bund::BundEvalError::Eval { msg, ms: success.ms },
                bund_settings,
            )));
        }
    };

    // Assemble the user-message turn.  If the operator left no
    // natural-language remainder, append a default instruction so
    // the LLM has SOMETHING to do with the result.
    let question = if snip.remainder.trim().is_empty() {
        "Summarise these results for the operator.".to_owned()
    } else {
        snip.remainder.clone()
    };
    // The enriched user-message MUST carry the snippet source verbatim
    // so future turns can pin it (see `chat::recent_bund_pins`).  The
    // leading "Bund snippet executed:" header is a stable marker the
    // pin extractor matches on.
    let enriched = if formatted.kind == "fingerprint" {
        format!(
            "Bund snippet executed:\n\n```bund\n{}\n```\n\n\
             Result ({} items, {} ms, fingerprinted because the JSON encoding \
             exceeded `llm.chat.bund.max_result_chars`):\n\n{}\n\n---\n\n\
             User question: {}",
            snip.code.trim(), success.items.len(), success.ms, formatted.body, question,
        )
    } else {
        format!(
            "Bund snippet executed:\n\n```bund\n{}\n```\n\n\
             Result ({} items, {} ms):\n\n```json\n{}\n```\n\n---\n\n\
             User question: {}",
            snip.code.trim(), success.items.len(), success.ms, formatted.body, question,
        )
    };

    // Auto-pick num_ctx based on assembled prompt size (same as the
    // RAG-path logic below).
    if options.num_ctx.is_none() {
        let approx = (enriched.len() + system_prompt.len()) / 4;
        options.num_ctx = Some(match approx {
            0..=4096       => 8192,
            4097..=8192    => 16384,
            8193..=16384   => 32768,
            _              => 65536,
        });
    }
    let prompt_chars   = enriched.len();
    let chosen_num_ctx = options.num_ctx;

    // chat_id resolution — same auto-recovery as the RAG path.
    let resume_id = match chat_id_str {
        Some(id_str) => {
            let chat_id = Uuid::parse_str(id_str).map_err(|e|
                err_msg(format!("vm::api::llm::chat: invalid chat_id {id_str:?}: {e}")))?;
            match llm_chat::session_metadata(chat_id) {
                Ok(Some(_)) => Some(chat_id),
                _           => None,
            }
        }
        None => None,
    };

    let outcome = match resume_id {
        Some(chat_id) => llm_chat::turn(chat_id, &enriched, provider_override,
                                        model_override, options)
            .map_err(|e| err_msg(format!("vm::api::llm::chat: {e}")))?,
        None => llm_chat::open_and_turn(provider_override, model_override,
                                        system_prompt, &enriched, options)
            .map_err(|e| err_msg(format!("vm::api::llm::chat: {e}")))?,
    };

    let cache_label = "disabled:chat";
    let bund_block = bund_stats_ok(snip, &success, &formatted, bund_settings);

    let _ = user_message;  // kept in signature for future audit logging
    let _ = req_json;

    let out = json!({
        "chat_id":         outcome.chat_id.to_string(),
        "response":        outcome.response,
        "provider":        outcome.provider,
        "model":           outcome.model,
        "is_new_session":  outcome.is_new_session,
        // The aggregationsearch path didn't run — these are 0 to make
        // it obvious in the response that the prompt context came
        // from the bund eval instead.
        "telemetry_count": 0,
        "document_count":  0,
        "prompt_chars":    prompt_chars,
        "num_ctx":         chosen_num_ctx,
        "finish_reason":   outcome.finish_reason,
        "tokens_in":       outcome.tokens_in,
        "tokens_out":      outcome.tokens_out,
        "ms":              outcome.ms,
        "cache":           cache_label,
        "bund":            bund_block,
    });
    meta::set_llm(json!({
        "provider":     outcome.provider,
        "model":        outcome.model,
        "ms":           outcome.ms,
        "tokens_in":    outcome.tokens_in,
        "tokens_out":   outcome.tokens_out,
        "prompt_chars": prompt_chars,
        "num_ctx":      chosen_num_ctx,
        "cache":        cache_label,
        "bund": {
            "ok":     true,
            "ms":     success.ms,
            "source": snip.source.as_str(),
            "n_items": success.items.len(),
            "result_kind":  formatted.kind,
            "result_chars": formatted.chars,
            "result_truncated": formatted.truncated,
        },
    }));
    Ok(json_to_dynamic(out))
}

/// Build the `bund` stats block emitted on a successful eval.
fn bund_stats_ok(
    snip:          &snippet::BundSnippet,
    success:       &chat_bund::BundEvalSuccess,
    formatted:     &chat_bund::FormattedResult,
    bund_settings: &chat_bund::ChatBundSettings,
) -> JsonValue {
    json!({
        "ok":                true,
        "ms":                success.ms,
        "source":            snip.source.as_str(),
        "code_chars":        snip.code.len(),
        "timeout_secs":      bund_settings.timeout_secs,
        "n_items":           success.items.len(),
        "result_kind":       formatted.kind,
        "result_chars":      formatted.chars,
        "result_truncated":  formatted.truncated,
        "cluster_meta":      success.cluster_meta.clone(),
    })
}

/// Build the early-return response for a failed snippet eval.
/// No LLM call happens; no chat history is touched.
fn early_return_bund_error(
    chat_id_str:   Option<&str>,
    snip:          &snippet::BundSnippet,
    err:           chat_bund::BundEvalError,
    bund_settings: &chat_bund::ChatBundSettings,
) -> JsonValue {
    let bund_block = json!({
        "ok":            false,
        "ms":            err.ms(),
        "source":        snip.source.as_str(),
        "code_chars":    snip.code.len(),
        "timeout_secs":  bund_settings.timeout_secs,
        "error": {
            "kind":    err.kind(),
            "message": err.message(),
        },
    });
    meta::set_llm(json!({
        "bund": bund_block.clone(),
        "cache": "disabled:chat",
    }));
    // No new chat_id minted — return the operator-supplied one if
    // any, or null.  Frontend can preserve the existing cookie.
    let chat_id_value = chat_id_str.map(JsonValue::from).unwrap_or(JsonValue::Null);
    json!({
        "chat_id":         chat_id_value,
        "response":        "",
        "is_new_session":  false,
        "telemetry_count": 0,
        "document_count":  0,
        "ms":              err.ms(),
        "cache":           "disabled:chat",
        "bund":            bund_block,
    })
}

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

    // Cluster-aware path: standalone collapses to a local call, cluster
    // mode fans out + merges across every Alive peer.  Operators who
    // were debugging "RAG didn't see my data" on a multi-node setup
    // were almost certainly hitting the old local-only path.
    let v = crate::vm::api::search::aggregation_search(dur, query)
        .map_err(|e| err_msg(format!("vm::api::llm::chat: aggregation_search: {e}")))?;
    let agg = dynamic_to_json(v);
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
    let summary = parts.join("\n");

    // Surface empty-context cases loudly — the operator was probably
    // expecting RAG to find something for this `duration` + `query`.
    if summary.is_empty() {
        log::warn!(
            "[llm::chat] RAG returned NO rows for duration={dur:?} query={query:?} \
             (telemetry={n_tel} docs={n_doc}) — the model will answer without \
             context.  Check that `cluster.full_replication_stores` and the \
             search index actually cover the queried window."
        );
    } else {
        log::info!(
            "[llm::chat] RAG loaded telemetry={n_tel} docs={n_doc} \
             chars={chars} for duration={dur:?} query={query:?}",
            chars = summary.len(),
        );
    }

    Ok((summary, n_tel, n_doc))
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
    let mut options = parse_options(&req_json);

    // Same auto-bump as chat — analyze can pack ContextSource rows
    // that exceed Ollama's 2048-token default num_ctx.  See the chat
    // helper comment for the bucketing rationale.
    if options.num_ctx.is_none() {
        let approx_tokens = (user_message.len() + system_prompt.len()) / 4;
        options.num_ctx = Some(match approx_tokens {
            0..=4096       => 8192,
            4097..=8192    => 16384,
            8193..=16384   => 32768,
            _              => 65536,
        });
    }

    // Build canonical request before deciding cache disposition.  The
    // fingerprints are sorted so two callers building the same context
    // from different row orders hash to the same key.
    let mut fps: Vec<String> = rag.rows.iter().map(|r| r.fingerprint.clone()).collect();
    fps.sort();
    let disposition = cache_disposition(&req_json, &options);
    let canonical = json!({
        "kind":               format!("analyze:{kind}"),
        "provider":           provider.id(),
        "model":              model.clone(),
        "source":             rag.source_meta.clone(),
        "fingerprints":       fps,
        "query":              query.clone(),
        "prompt_template":    prompt_template.clone(),
        "system_prompt":      system_prompt.clone(),
        "options":            options_to_canonical(&options),
    });
    let key = cache::cache_key(&canonical);

    if let Some(entry) = cache_lookup(&disposition, &key) {
        let mut out = cache_hit_response(&entry, "analyze", None,
                                         Some(rag.source_meta.clone()));
        out["kind"]   = json!(kind);
        out["n_rows"] = json!(rag.n_rows);
        return Ok(json_to_dynamic(out));
    }

    // Phase 4 dedup gate (same shape as `complete`).
    let lease_outcome = try_acquire_lease(&key);
    if matches!(lease_outcome, LeaseOutcome::SkipRunning | LeaseOutcome::SkipDone) {
        if let Some(entry) = wait_for_cache_entry(&disposition, &key) {
            let mut out = cache_hit_response(&entry, "analyze",
                                             Some(lease_outcome.label()),
                                             Some(rag.source_meta.clone()));
            out["kind"]   = json!(kind);
            out["n_rows"] = json!(rag.n_rows);
            return Ok(json_to_dynamic(out));
        }
    }
    let dedup_label = lease_outcome.label();

    let rq = CompletionRequest {
        model:    model.clone(),
        messages: vec![Message::system(&system_prompt), Message::user(&user_message)],
        options,
    };

    let started = Instant::now();
    let resp_result = runtime::block_on(provider.complete(rq));
    let ms = started.elapsed().as_millis() as u64;

    let resp: CompletionResponse = match (resp_result, lease_outcome) {
        (Ok(r), LeaseOutcome::Acquired(lease))  => { lease.release_done();   r }
        (Err(e), LeaseOutcome::Acquired(lease)) => { lease.release_failed();
            return Err(err_msg(format!("vm::api::llm::analyze: provider {:?}: {e}",
                provider.id()))); }
        (Ok(r),  _) => r,
        (Err(e), _) => return Err(err_msg(format!(
            "vm::api::llm::analyze: provider {:?}: {e}", provider.id()))),
    };

    let miss_label = disposition.miss_label();
    cache_store(&disposition, &key, provider.id(), &resp.model,
                &format!("analyze:{kind}"),
                canonical, response_to_cache(&resp),
                Some(rag.source_meta.clone()));

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
        "cache":         miss_label,
        "dedup":         dedup_label,
    });
    meta::set_llm(json!({
        "provider":   provider.id(),
        "model":      model,
        "ms":         ms,
        "tokens_in":  out["tokens_in"].clone(),
        "tokens_out": out["tokens_out"].clone(),
        "kind":       kind,
        "n_rows":     rag.n_rows,
        "cache":      miss_label,
        "dedup":      dedup_label,
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

// ─────────────────────────────────────────────────────────────────────
// Async helpers + job management
// ─────────────────────────────────────────────────────────────────────

/// `complete_async` — enqueue a [`complete`]-shaped request for the
/// background runner.  Returns `{job_id, result_id, kind: "complete"}`.
/// Callers poll `v2/results.pull` against `result_id` to receive the
/// final response when the runner finishes.
pub fn complete_async(req: Value) -> Result<Value, Error> {
    meta::clear_llm();
    let req_json = req_as_object(req, "vm::api::llm::complete_async")?;
    // Sanity-check the same way the sync helper would so the caller
    // gets a fast error before the runner ever picks it up.
    let _ = parse_messages(&req_json)?;
    enqueue_job("complete", req_json)
}

/// `analyze_async` — enqueue an [`analyze`]-shaped request.  Returns
/// `{job_id, result_id, kind: "analyze:<kind>"}` so the caller knows
/// which analyze variant they queued.
pub fn analyze_async(req: Value) -> Result<Value, Error> {
    meta::clear_llm();
    let req_json = req_as_object(req, "vm::api::llm::analyze_async")?;
    let kind = req_json.get("kind").and_then(|v| v.as_str())
        .ok_or_else(|| err_msg("vm::api::llm::analyze_async: `kind` (string) is required"))?
        .to_owned();
    // Sanity-check: builds a ContextSource but doesn't actually
    // execute it.  Catches bad params (e.g. kind=documents without ids)
    // synchronously.
    let _ = build_context_source(&kind, &req_json)?;
    enqueue_job(&format!("analyze:{kind}"), req_json)
}

fn enqueue_job(kind: &str, req_json: JsonValue) -> Result<Value, Error> {
    let q = jobs::queue().ok_or_else(|| err_msg(
        "vm::api::llm: job queue not initialised (no llm config or runner disabled)"))?;
    let result_id = req_json.get("result_id").and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let (job_id, result_id) = q.enqueue(JobInsert {
        kind:         kind.to_owned(),
        request_json: req_json,
        result_id,
    }).map_err(|e| err_msg(format!("vm::api::llm: enqueue: {e}")))?;
    Ok(json_to_dynamic(json!({
        "job_id":    job_id.to_string(),
        "result_id": result_id.to_string(),
        "kind":      kind,
        "state":     "pending",
    })))
}

/// `job_status` — return the current state of a queued job.  Accepts
/// either a string UUID or a Map with `{job_id: "..."}`.
pub fn job_status(req: Value) -> Result<Value, Error> {
    let id = extract_job_id(req, "vm::api::llm::job_status")?;
    let q = jobs::queue().ok_or_else(|| err_msg(
        "vm::api::llm::job_status: job queue not initialised"))?;
    let row = q.get(id).map_err(|e| err_msg(format!("vm::api::llm::job_status: {e}")))?
        .ok_or_else(|| err_msg(format!("vm::api::llm::job_status: job {id} not found")))?;
    Ok(json_to_dynamic(job_to_summary_json(&row)))
}

/// `job_cancel` — mark a pending/running job as cancelled.  Returns
/// `{ok: true}` when the cancellation took effect, `{ok: false}` when
/// the job was already terminal or doesn't exist.
pub fn job_cancel(req: Value) -> Result<Value, Error> {
    let id = extract_job_id(req, "vm::api::llm::job_cancel")?;
    let q = jobs::queue().ok_or_else(|| err_msg(
        "vm::api::llm::job_cancel: job queue not initialised"))?;
    let ok = q.cancel(id).map_err(|e| err_msg(format!("vm::api::llm::job_cancel: {e}")))?;
    Ok(json_to_dynamic(json!({ "ok": ok, "job_id": id.to_string() })))
}

/// `jobs_list` — list queued / in-flight / recently-finished jobs.
///
/// Accepts a Map filter:
/// - `state`  (optional string)  — restrict to one state
/// - `limit`  (optional integer) — cap on returned rows (default 100)
///
/// Returns `{jobs: [...summary...], count}`.
pub fn jobs_list(req: Value) -> Result<Value, Error> {
    let req_json = match req.data {
        rust_dynamic::types::Val::Null | rust_dynamic::types::Val::Exit => JsonValue::Null,
        _ => dynamic_to_json(req),
    };
    let state = req_json.get("state").and_then(|v| v.as_str())
        .and_then(JobState::from_wire);
    let limit = req_json.get("limit").and_then(|v| v.as_u64())
        .map(|n| n as usize).or(Some(100));

    let q = jobs::queue().ok_or_else(|| err_msg(
        "vm::api::llm::jobs_list: job queue not initialised"))?;
    let rows = q.list(JobListFilter { state, limit })
        .map_err(|e| err_msg(format!("vm::api::llm::jobs_list: {e}")))?;
    let count = rows.len();
    let jobs: Vec<JsonValue> = rows.iter().map(job_to_summary_json).collect();
    Ok(json_to_dynamic(json!({ "jobs": jobs, "count": count })))
}

fn extract_job_id(req: Value, ctx: &str) -> Result<Uuid, Error> {
    let json = dynamic_to_json(req);
    let s = if let Some(s) = json.as_str() {
        s
    } else if let Some(s) = json.get("job_id").and_then(|v| v.as_str()) {
        s
    } else {
        bail!("{ctx}: expected a UUID string or {{job_id: \"...\"}} map");
    };
    Uuid::parse_str(s).map_err(|e| err_msg(format!("{ctx}: invalid job_id {s:?}: {e}")))
}

fn job_to_summary_json(row: &jobs::Job) -> JsonValue {
    json!({
        "job_id":       row.job_id.to_string(),
        "result_id":    row.result_id.to_string(),
        "kind":         row.kind,
        "state":        row.state.as_str(),
        "owner_node":   row.owner_node.map(|u| u.to_string()),
        "submitted_at": row.submitted_at,
        "started_at":   row.started_at,
        "finished_at":  row.finished_at,
        "error":        row.error,
    })
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
