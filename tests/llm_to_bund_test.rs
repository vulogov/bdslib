//! End-to-end tests for `bdslib::llm::to_bund::translate`.
//!
//! Uses a custom [`Provider`] impl (`ScriptedProvider`) that pops
//! its assistant response from a per-test scripted queue.  This lets
//! us control exactly what the LLM returns on each turn without
//! booting an HTTP mock — and lets us register two providers under
//! different names + ids to test the per-call override.
//!
//! Coverage:
//! 1. Round-trip success on a valid first-try fenced ```bund``` block.
//! 2. Retry-then-success when the first turn is unparseable Bund and
//!    the second turn fixes it — proves the parse-error feedback
//!    loop actually advances the conversation.
//! 3. Hard failure after `max_retries=2` exhausts (3 total tries).
//! 4. Per-call `provider` override resolves a different registered
//!    provider when the default would have picked the other one.

use async_trait::async_trait;
use bdslib::common::error::{err_msg, Result as BResult};
use bdslib::llm::manager::{self, ProviderManager};
use bdslib::llm::providers::Provider;
use bdslib::llm::to_bund::{self, ToBundSettings};
use bdslib::llm::types::{Capabilities, CompletionRequest, CompletionResponse};
use parking_lot::Mutex;
use serde_json::json;
use std::sync::{Arc, OnceLock};

// ─────────────────────────────────────────────────────────────────────
// ScriptedProvider — pops the next assistant text from a shared queue
// ─────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct ProviderInbox {
    /// Pre-scripted assistant texts; consumed FIFO from the back.
    queue:    Mutex<Vec<String>>,
    /// CompletionRequests this provider saw (for retry-loop asserts).
    requests: Mutex<Vec<CompletionRequest>>,
}

struct ScriptedProvider {
    id:            &'static str,
    default_model: String,
    inbox:         Arc<ProviderInbox>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &str { self.id }
    fn default_model(&self) -> &str { &self.default_model }
    fn capabilities(&self) -> Capabilities { Capabilities { chat: true, embed: false } }

    async fn complete(&self, req: CompletionRequest) -> BResult<CompletionResponse> {
        self.inbox.requests.lock().push(req.clone());
        let next = self.inbox.queue.lock().pop()
            .ok_or_else(|| err_msg(format!(
                "ScriptedProvider {:?}: queue empty (test forgot to push a response)",
                self.id
            )))?;
        Ok(CompletionResponse {
            text:          next,
            model:         req.model,
            finish_reason: Some("stop".into()),
            tokens_in:     Some(10),
            tokens_out:    Some(5),
        })
    }
}

fn primary_inbox() -> Arc<ProviderInbox> {
    static I: OnceLock<Arc<ProviderInbox>> = OnceLock::new();
    I.get_or_init(|| Arc::new(ProviderInbox::default())).clone()
}

fn alt_inbox() -> Arc<ProviderInbox> {
    static I: OnceLock<Arc<ProviderInbox>> = OnceLock::new();
    I.get_or_init(|| Arc::new(ProviderInbox::default())).clone()
}

/// One-time install: two named providers (`primary` is the default,
/// `alt` is the override target) sharing a tokio runtime via the
/// vm::api::runtime helper that `to_bund::translate` uses.
fn ensure_setup() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let primary = ScriptedProvider {
            id: "primary", default_model: "primary-model".into(),
            inbox: primary_inbox(),
        };
        let alt = ScriptedProvider {
            id: "alt", default_model: "alt-model".into(),
            inbox: alt_inbox(),
        };
        let mut mgr = ProviderManager::empty(Some("primary".into()));
        mgr.insert("primary", Arc::new(primary));
        mgr.insert("alt",     Arc::new(alt));
        manager::init(mgr);

        to_bund::init_settings(ToBundSettings {
            enabled:             true,
            timeout_secs:        30,
            max_retries:         2,
            provider:            String::new(),
            model:               String::new(),
            extra_system_prompt: String::new(),
        });

        // Init the Adam VM so the undefined-word dry-run has a
        // non-empty known-word set to check against.  Without this
        // the check degrades to a no-op and Phase 2 tests below
        // can't verify the retry path.
        bdslib::init_adam().expect("init_adam");
    });
}

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static M: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

fn reset_inboxes() {
    for ib in [primary_inbox(), alt_inbox()] {
        ib.queue.lock().clear();
        ib.requests.lock().clear();
    }
}

/// Push a response onto the FRONT of the queue so it pops in FIFO
/// order (later pushes pop later).
fn push_response(inbox: &Arc<ProviderInbox>, body: impl Into<String>) {
    inbox.queue.lock().insert(0, body.into());
}

// ─────────────────────────────────────────────────────────────────────

#[test]
fn translate_round_trip_success_first_try() {
    ensure_setup();
    let _g = test_lock();
    reset_inboxes();

    push_response(&primary_inbox(),
        "Here's the script:\n```bund\n42 println\n```\nDone!");

    let t = to_bund::translate("print 42", &json!({})).expect("translate");
    assert!(t.valid, "expected valid translation, got {:?}", t);
    assert_eq!(t.parse_attempts, 1);
    assert!(t.parse_error.is_none());
    assert!(t.script.contains("42 println"));
    assert_eq!(t.provider, "primary");
    assert_eq!(t.model,    "primary-model");
    // Exactly one round trip — no retry was needed.
    assert_eq!(primary_inbox().requests.lock().len(), 1);
    // Initial conversation: system + user (no retries).
    let req = primary_inbox().requests.lock().first().cloned().unwrap();
    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.messages[0].role.as_str(), "system");
    assert_eq!(req.messages[1].role.as_str(), "user");
    assert!(req.messages[1].content.contains("print 42"));
}

#[test]
fn translate_recovers_after_one_parse_failure() {
    ensure_setup();
    let _g = test_lock();
    reset_inboxes();

    // First pop: deliberately unparseable Bund (unterminated string).
    // Second pop: valid fixed-up script.
    push_response(&primary_inbox(), "```bund\n\"hello\n```");
    push_response(&primary_inbox(), "```bund\n1 2 + println\n```");

    let t = to_bund::translate("add 1 and 2", &json!({})).expect("translate");
    assert!(t.valid, "expected eventual success, got {:?}", t);
    assert_eq!(t.parse_attempts, 2);
    assert!(t.parse_error.is_none());
    assert!(t.script.contains("1 2 + println"));

    let inbox = primary_inbox();
    let reqs = inbox.requests.lock();
    assert_eq!(reqs.len(), 2, "expected one retry");

    // Confirm the retry conversation seeded the parse error.
    // After one retry: [system, user, assistant(bad), user(error)].
    let retry = &reqs[1];
    assert_eq!(retry.messages.len(), 4);
    assert_eq!(retry.messages[2].role.as_str(), "assistant");
    assert!(retry.messages[2].content.contains("\"hello"));
    assert_eq!(retry.messages[3].role.as_str(), "user");
    assert!(retry.messages[3].content.to_lowercase().contains("failed to validate"),
        "retry user turn should restate the validation failure, got: {}",
        retry.messages[3].content);
}

#[test]
fn translate_returns_invalid_after_exhausting_retries() {
    ensure_setup();
    let _g = test_lock();
    reset_inboxes();

    // Every turn returns unparseable Bund.  With max_retries=2 the
    // translator should make 3 total attempts before giving up.
    push_response(&primary_inbox(), "```bund\n\"unterminated\n```");
    push_response(&primary_inbox(), "```bund\n[ 1 2 3\n```");
    push_response(&primary_inbox(), "```bund\n\"still broken\n```");

    let t = to_bund::translate("do something", &json!({})).expect("translate");
    assert!(!t.valid, "expected invalid translation, got {:?}", t);
    assert_eq!(t.parse_attempts, 3);
    assert!(t.parse_error.is_some(),
        "parse_error must be populated on final failure");
    assert!(!t.script.trim().is_empty(),
        "script field should still carry the last attempted body");
    assert_eq!(primary_inbox().requests.lock().len(), 3);
}

#[test]
fn translate_honours_per_call_provider_override() {
    ensure_setup();
    let _g = test_lock();
    reset_inboxes();

    push_response(&alt_inbox(), "```bund\n7 println\n```");

    let req_extra = json!({"provider": "alt"});
    let t = to_bund::translate("print seven", &req_extra).expect("translate");
    assert!(t.valid);
    assert_eq!(t.provider, "alt",
        "explicit provider override should win over the manager default");
    assert_eq!(t.model, "alt-model",
        "no model override → provider's default_model");
    // Primary saw no traffic.
    assert_eq!(primary_inbox().requests.lock().len(), 0);
    assert_eq!(alt_inbox().requests.lock().len(),     1);
}

// ─────────────────────────────────────────────────────────────────────
// Phase 2 — undefined-word dry-run
// ─────────────────────────────────────────────────────────────────────

/// First turn references a word the Adam VM has never heard of
/// (`make.unicorn`) but parses cleanly.  The dry-run must catch it
/// and feed the error back so the second turn can correct it.
#[test]
fn translate_rejects_unknown_word_then_recovers() {
    ensure_setup();
    let _g = test_lock();
    reset_inboxes();

    // First pop: syntactically valid Bund using a totally fake word.
    // Second pop: same intent expressed with real stdlib words.
    push_response(&primary_inbox(), "```bund\n42 make.unicorn\n```");
    push_response(&primary_inbox(), "```bund\n42 println\n```");

    let t = to_bund::translate("print 42", &json!({})).expect("translate");
    assert!(t.valid, "expected eventual success, got {:?}", t);
    assert_eq!(t.parse_attempts, 2);
    assert!(t.script.contains("42 println"));

    let inbox = primary_inbox();
    let reqs  = inbox.requests.lock();
    assert_eq!(reqs.len(), 2);

    // The retry user-turn must mention the unknown word so the model
    // knows what to fix.
    let retry_user = &reqs[1].messages.last().expect("retry user turn").content;
    assert!(retry_user.to_lowercase().contains("not registered"),
        "retry should announce the unregistered-word failure, got: {retry_user}");
    assert!(retry_user.contains("make.unicorn"),
        "retry should quote the specific unknown word, got: {retry_user}");
}

/// When every attempt references unknown words, the translator
/// exhausts `max_retries` and returns `valid=false` with the unknown
/// list embedded in `parse_error`.
#[test]
fn translate_unknown_word_exhausts_retries() {
    ensure_setup();
    let _g = test_lock();
    reset_inboxes();

    push_response(&primary_inbox(), "```bund\nmake.unicorn\n```");
    push_response(&primary_inbox(), "```bund\nride.dragon\n```");
    push_response(&primary_inbox(), "```bund\nsummon.kraken\n```");

    let t = to_bund::translate("do nonsense", &json!({})).expect("translate");
    assert!(!t.valid, "expected invalid translation, got {:?}", t);
    assert_eq!(t.parse_attempts, 3);
    let err = t.parse_error.as_deref().unwrap_or("");
    assert!(err.contains("not registered"),
        "final parse_error should announce unknown words, got: {err}");
    assert!(err.contains("summon.kraken"),
        "final parse_error should reference the last attempt's word, got: {err}");
}
