//! Provider-agnostic chat sessions.
//!
//! Each session is one docstore document whose **content** is a JSON-
//! encoded array of [`crate::llm::types::Message`] turns and whose
//! **metadata** records the binding to a specific provider + model +
//! system prompt:
//!
//! ```json
//! { "type":          "llm_chat_session",
//!   "provider":      "anthropic",
//!   "model":         "claude-sonnet-4-5",
//!   "system_prompt": "…" }
//! ```
//!
//! The provider name is persisted so subsequent turns are routed to the
//! same upstream even when the caller doesn't repeat it.  Callers can
//! still override `provider` / `model` per turn — the override sticks
//! for that turn only and is *not* written back to the session metadata.
//!
//! Replaces `crate::ai::ollama::{new_chat_session, chat}`, which was
//! hard-bound to Ollama.  The legacy module stays for now as the
//! backing store of `v2/chat.ollama`; once bdsweb migrates to
//! `v4/llm.chat` the legacy path can be deleted.

use crate::common::error::{err_msg, Result};
use crate::globals::get_db;
use crate::llm::manager;
use crate::llm::types::{CompletionOpts, CompletionRequest, CompletionResponse, Message, Role};
use serde_json::{json, Value as JsonValue};
use std::time::Instant;
use uuid::Uuid;

const META_TYPE: &str = "llm_chat_session";

/// Outcome of one [`turn`] call.  All fields except `response` are
/// metadata the caller can surface alongside the assistant reply.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub chat_id:        Uuid,
    pub response:       String,
    pub provider:       String,
    pub model:          String,
    pub is_new_session: bool,
    pub tokens_in:      Option<u32>,
    pub tokens_out:     Option<u32>,
    pub finish_reason:  Option<String>,
    pub ms:             u64,
}

/// Create a new chat session bound to `provider` + `model` and seed it
/// with a system message.  Returns the docstore UUID — pass it to
/// [`turn`] for every subsequent message in the conversation.
pub fn new_session(provider: &str, model: &str, system_prompt: &str) -> Result<Uuid> {
    let db = get_db()?;
    let metadata = json!({
        "type":          META_TYPE,
        "provider":      provider,
        "model":         model,
        "system_prompt": system_prompt,
    });
    let initial = vec![Message::system(system_prompt)];
    let content = serde_json::to_vec(&initial)
        .map_err(|e| err_msg(format!("llm::chat: serialize initial history: {e}")))?;
    db.doc_add(metadata, &content)
}

/// Send `user_message` in session `chat_id`, persist the updated
/// history, and return the assistant reply along with provider/model
/// metadata.
///
/// `provider_override` / `model_override` apply to this turn only;
/// they're not written back to the session metadata.  When `None`,
/// the values stored in the session metadata are used; if the session
/// has no recorded provider (legacy sessions), the manager's default
/// is consulted.
pub fn turn(
    chat_id:           Uuid,
    user_message:      &str,
    provider_override: Option<&str>,
    model_override:    Option<&str>,
    options:           CompletionOpts,
) -> Result<TurnOutcome> {
    let db = get_db()?;

    let metadata = db.doc_get_metadata(chat_id)?
        .ok_or_else(|| err_msg(format!("llm::chat: session {chat_id} not found")))?;
    let raw_history = db.doc_get_content(chat_id)?.unwrap_or_default();

    let mut history: Vec<Message> = if raw_history.is_empty() {
        vec![]
    } else {
        serde_json::from_slice(&raw_history)
            .map_err(|e| err_msg(format!("llm::chat: deserialize history: {e}")))?
    };

    // Resolve provider: explicit override > session metadata > manager default.
    let provider_name = provider_override
        .map(str::to_owned)
        .or_else(|| metadata.get("provider").and_then(|v| v.as_str()).map(str::to_owned));
    let mgr = manager::manager()
        .ok_or_else(|| err_msg("llm::chat: provider manager not initialised"))?;
    let provider = mgr.resolve(provider_name.as_deref())
        .map_err(|e| err_msg(format!("llm::chat: {e}")))?;

    let model = model_override
        .map(str::to_owned)
        .or_else(|| metadata.get("model").and_then(|v| v.as_str()).map(str::to_owned))
        .unwrap_or_else(|| provider.default_model().to_owned());

    // Ensure the session's system prompt is the first message.
    if history.first().map(|m| m.role != Role::System).unwrap_or(true) {
        let sys = metadata.get("system_prompt").and_then(|v| v.as_str()).unwrap_or("");
        history.insert(0, Message::system(sys));
    }
    history.push(Message::user(user_message));

    let rq = CompletionRequest {
        model:    model.clone(),
        messages: history.clone(),
        options,
    };

    let started = Instant::now();
    let resp: CompletionResponse = crate::vm::api::runtime::block_on(provider.complete(rq))
        .map_err(|e| err_msg(format!("llm::chat: provider {:?}: {e}", provider.id())))?;
    let ms = started.elapsed().as_millis() as u64;

    history.push(Message::assistant(resp.text.clone()));
    let updated = serde_json::to_vec(&history)
        .map_err(|e| err_msg(format!("llm::chat: serialize updated history: {e}")))?;
    db.doc_update_content(chat_id, &updated)?;

    Ok(TurnOutcome {
        chat_id,
        response:       resp.text,
        provider:       provider.id().to_owned(),
        model:          resp.model,
        is_new_session: false,
        tokens_in:      resp.tokens_in,
        tokens_out:     resp.tokens_out,
        finish_reason:  resp.finish_reason,
        ms,
    })
}

/// Helper used by [`crate::vm::api::llm::chat`] to support the
/// "first turn auto-creates the session" path.  Mints a session,
/// records `is_new_session=true` on the outcome, then immediately
/// runs one [`turn`].
pub fn open_and_turn(
    provider:      Option<&str>,
    model:         Option<&str>,
    system_prompt: &str,
    user_message:  &str,
    options:       CompletionOpts,
) -> Result<TurnOutcome> {
    let mgr = manager::manager()
        .ok_or_else(|| err_msg("llm::chat: provider manager not initialised"))?;
    let p = mgr.resolve(provider).map_err(|e| err_msg(format!("llm::chat: {e}")))?;
    let provider_id = p.id().to_owned();
    let resolved_model = model.map(str::to_owned)
        .unwrap_or_else(|| p.default_model().to_owned());
    let chat_id = new_session(&provider_id, &resolved_model, system_prompt)?;
    let mut outcome = turn(chat_id, user_message, Some(&provider_id), Some(&resolved_model), options)?;
    outcome.is_new_session = true;
    Ok(outcome)
}

/// One Bund-result entry pulled from a chat session's history.  Used
/// by [`recent_bund_pins`] to surface prior snippet runs into the
/// next turn's prompt so a follow-up question like "what was the
/// answer?" can be answered without re-running the snippet.
#[derive(Debug, Clone)]
pub struct BundPin {
    /// Bund source code the operator submitted (verbatim, trimmed).
    pub code:    String,
    /// The result body as written into the enriched user-message —
    /// either a `\`\`\`json` block or a fingerprint line.  Truncated to
    /// the caller's `max_chars_per_pin` budget with a trailing "…"
    /// marker when oversize.
    pub result:  String,
    /// The assistant reply that immediately followed this snippet
    /// turn, truncated the same way.  Empty when there was no reply
    /// (shouldn't happen in normal flow but the extractor is
    /// defensive).
    pub summary: String,
}

/// Mine a chat session's persisted history for the most recent
/// Bund-snippet runs.  Returns at most `max_results` pins in
/// chronological order (oldest → newest), each truncated to
/// `max_chars_per_pin`.  Skips silently and returns an empty Vec for
/// any error condition (missing session, malformed history) — the
/// caller treats this as a best-effort enrichment.
///
/// The extractor matches the stable header written by
/// [`crate::vm::api::llm::run_chat_with_bund_result`]:
///
/// ```text
/// Bund snippet executed:
///
/// ```bund
/// <source>
/// ```
///
/// Result (N items, M ms):
///
/// ```json
/// <items>
/// ```
///
/// ---
///
/// User question: ...
/// ```
pub fn recent_bund_pins(
    chat_id:           Uuid,
    max_results:       usize,
    max_chars_per_pin: usize,
) -> Result<Vec<BundPin>> {
    if max_results == 0 { return Ok(Vec::new()); }
    let db = get_db()?;
    let raw = match db.doc_get_content(chat_id) {
        Ok(Some(b)) => b,
        _           => return Ok(Vec::new()),
    };
    let history: Vec<Message> = match serde_json::from_slice(&raw) {
        Ok(h) => h,
        Err(_) => return Ok(Vec::new()),
    };

    let trunc = |s: &str| -> String {
        if s.chars().count() <= max_chars_per_pin {
            s.to_owned()
        } else {
            let cut: String = s.chars().take(max_chars_per_pin).collect();
            format!("{cut}…")
        }
    };

    let mut pins: Vec<BundPin> = Vec::new();
    for (i, msg) in history.iter().enumerate() {
        if msg.role != Role::User { continue; }
        let txt = &msg.content;
        if !txt.starts_with("Bund snippet executed:") { continue; }

        // Extract source between ```bund and the next ``` line.
        let Some(src_start) = txt.find("```bund\n") else { continue };
        let after_marker   = &txt[src_start + "```bund\n".len()..];
        let Some(src_end)  = after_marker.find("\n```") else { continue };
        let code = after_marker[..src_end].trim().to_owned();

        // Extract result block (between the first "Result (" header and
        // the "\n---\n" separator that precedes the User question).
        let rest = &after_marker[src_end + "\n```".len()..];
        let result_body = match rest.find("\n---\n") {
            Some(end) => rest[..end].trim().to_owned(),
            None      => rest.trim().to_owned(),
        };

        // Companion assistant reply, if any.
        let summary = history.get(i + 1)
            .filter(|m| m.role == Role::Assistant)
            .map(|m| m.content.clone())
            .unwrap_or_default();

        pins.push(BundPin {
            code:    trunc(&code),
            result:  trunc(&result_body),
            summary: trunc(&summary),
        });
    }

    // Keep only the most recent N (chronological order preserved).
    let drop = pins.len().saturating_sub(max_results);
    Ok(pins.into_iter().skip(drop).collect())
}

/// Returns the metadata block for an existing session, or `None` when
/// the id doesn't reference a chat-session document.  Used by the
/// `v4/llm.chat` handler to surface provider/model info to clients
/// without forcing a turn.
pub fn session_metadata(chat_id: Uuid) -> Result<Option<JsonValue>> {
    let db = get_db()?;
    let md = db.doc_get_metadata(chat_id)?;
    Ok(md.and_then(|m| {
        if m.get("type").and_then(|v| v.as_str()) == Some(META_TYPE) {
            Some(m)
        } else {
            None
        }
    }))
}
