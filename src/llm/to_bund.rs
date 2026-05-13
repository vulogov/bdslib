//! `v2/to.bund` — LLM-based English → Bund translator.
//!
//! The endpoint hands a natural-language request to the default
//! provider, expects a fenced ```bund …``` block back, parses it
//! through `bund_language_parser::bund_parse` to verify syntax, and
//! returns the result.  Parse failures trigger up to
//! [`ToBundSettings::max_retries`] follow-up turns where the parse
//! error is fed back to the model with an "edit, don't rewrite"
//! instruction.
//!
//! This module is intentionally narrow:
//!
//! - It does **not** execute the generated script.  Even a sandboxed
//!   eval would let through `cls.delete` etc.; the caller decides
//!   whether to run.
//! - It does **not** introspect the live policy yet (that's Phase 2,
//!   along with the undefined-word dry-run).
//! - It does **not** support cluster fan-out (`v3/to.bund`); the call
//!   is single-node by construction.

use crate::common::error::{err_msg, Result};
use crate::llm::manager;
use crate::llm::to_bund_prompt::assemble_system_prompt_with_policy;
use crate::llm::types::{CompletionOpts, CompletionRequest, Message, Role};
use crate::vm::api::runtime;
use crate::vm::policy;
use crate::vm::registered_word_names;
use bund_language_parser::bund_parse;
use rust_dynamic::types::{CALL, Val};
use rust_dynamic::value::Value as DynValue;
use serde_json::{json, Value as JsonValue};
use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::time::Instant;

// ─────────────────────────────────────────────────────────────────────
// Runtime settings (parsed from `llm.to_bund.*` once at bdsnode startup)
// ─────────────────────────────────────────────────────────────────────

/// Operator-tunable knobs for the translator.  Populated once at
/// startup via [`init_settings`]; consulted on every call.
#[derive(Debug, Clone)]
pub struct ToBundSettings {
    /// Master switch.  When false, `v2/to.bund` returns an error
    /// without contacting the LLM.  Default: true.
    pub enabled:             bool,
    /// Per-request reqwest timeout passed through to the provider.
    /// Default: 120 s — bigger than `llm.chat.bund` because the
    /// translator's prompt is ~12-20k chars and the model has to do
    /// real work.
    pub timeout_secs:        u64,
    /// Maximum number of parse-validation retries.  Each retry
    /// appends the parse error to the conversation and asks the
    /// model to fix it.  Default: 2.  Floor 0, ceiling 5.
    pub max_retries:         usize,
    /// Optional provider override (`""` = use manager's default).
    pub provider:            String,
    /// Optional model override (`""` = use provider's default).
    pub model:               String,
    /// Free-form text appended to the baked system prompt so
    /// operators can layer site-specific guidance ("always use 1h
    /// durations", "tag every record env=prod", …).
    pub extra_system_prompt: String,
}

impl Default for ToBundSettings {
    fn default() -> Self {
        Self {
            enabled:             true,
            timeout_secs:        120,
            max_retries:         2,
            provider:            String::new(),
            model:               String::new(),
            extra_system_prompt: String::new(),
        }
    }
}

static SETTINGS: OnceLock<ToBundSettings> = OnceLock::new();

/// Install the process-wide translator settings.  Idempotent —
/// subsequent calls silently no-op (matches the rest of bdslib's
/// `init_*` conventions).
pub fn init_settings(s: ToBundSettings) { let _ = SETTINGS.set(s); }

/// Read the active settings.  Falls back to the default impl when
/// [`init_settings`] was never called.
pub fn settings() -> &'static ToBundSettings {
    SETTINGS.get_or_init(ToBundSettings::default)
}

// ─────────────────────────────────────────────────────────────────────
// Public API — what the JSON-RPC handler calls
// ─────────────────────────────────────────────────────────────────────

/// One call's worth of output.
#[derive(Debug, Clone)]
pub struct Translation {
    pub script:         String,
    pub valid:          bool,
    /// `1` on first-try success, `n+1` when the n-th retry finally
    /// produced valid output, or `max_retries+1` after final failure.
    pub parse_attempts: usize,
    /// `None` on success; `Some(message)` after a final failure.
    pub parse_error:    Option<String>,
    pub provider:       String,
    pub model:          String,
    pub ms:             u64,
    pub tokens_in:      Option<u32>,
    pub tokens_out:     Option<u32>,
}

/// Translate `message` (English) into a Bund script.  The
/// `req_extra` JSON object lets the caller pass through per-call
/// overrides without exploding the function signature; recognised
/// keys: `provider`, `model`, `max_retries`, `options` (any
/// [`CompletionOpts`] field).
pub fn translate(message: &str, req_extra: &JsonValue) -> Result<Translation> {
    let cfg = settings();
    if !cfg.enabled {
        return Err(err_msg("v2/to.bund: translator disabled by operator config \
                            (llm.to_bund.enabled = false)"));
    }
    let message = message.trim();
    if message.is_empty() {
        return Err(err_msg("v2/to.bund: `message` is required and must be non-empty"));
    }

    // Per-call overrides — fall back to baked settings.
    let provider_name = req_extra.get("provider").and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| cfg.provider.clone());
    let model_override = req_extra.get("model").and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| cfg.model.clone());
    let max_retries = req_extra.get("max_retries").and_then(|v| v.as_u64())
        .map(|n| (n as usize).min(5))
        .unwrap_or(cfg.max_retries);

    let mgr = manager::manager()
        .ok_or_else(|| err_msg("v2/to.bund: provider manager not initialised"))?;
    let resolver = if provider_name.is_empty() { None } else { Some(provider_name.as_str()) };
    let provider = mgr.resolve(resolver)
        .map_err(|e| err_msg(format!("v2/to.bund: {e}")))?;
    let model = if model_override.is_empty() {
        provider.default_model().to_owned()
    } else {
        model_override
    };

    // Splice the active sandbox policy into the prompt so the model
    // avoids words that would be denied at runtime.  Empty when no
    // policy is active (the common case for dev/test).
    let disabled_groups = policy::effective_disabled_by_category();
    let system_prompt = assemble_system_prompt_with_policy(
        &cfg.extra_system_prompt,
        &disabled_groups,
    );

    // Snapshot the active VM's registered-word set ONCE — used by the
    // undefined-word dry-run after each parse-successful attempt.  An
    // empty set (Adam not initialised) skips the check entirely so
    // tests / library users that bypass `init_adam` still work.
    let known_words = registered_word_names();

    // Conversation seed — system + user.  Retries append assistant +
    // a fresh user turn carrying the parse error.
    let mut messages: Vec<Message> = Vec::with_capacity(2 + max_retries * 2);
    messages.push(Message::system(&system_prompt));
    messages.push(Message::user(message));

    // Per-call options.  We auto-bump `num_ctx` because the system
    // prompt alone is ~15k chars and we want the model to actually
    // see it.  Caller's options override this.
    let mut options = parse_options(req_extra);
    if options.num_ctx.is_none() {
        let approx_tokens = system_prompt.len() / 4 + message.len() / 4 + 1024;
        options.num_ctx = Some(match approx_tokens {
            0..=8192        => 16384,
            8193..=16384    => 32768,
            _               => 65536,
        });
    }

    let started = Instant::now();
    let mut last_response_model: String = model.clone();
    let mut tokens_in_total:  Option<u32> = None;
    let mut tokens_out_total: Option<u32> = None;
    let mut last_raw_text  = String::new();
    let mut last_script    = String::new();
    let mut last_parse_err = String::new();

    for attempt in 0..=max_retries {
        let rq = CompletionRequest {
            model:    model.clone(),
            messages: messages.clone(),
            options:  options.clone(),
        };
        let resp = runtime::block_on(provider.complete(rq))
            .map_err(|e| err_msg(format!("v2/to.bund: provider {:?}: {e}", provider.id())))?;
        last_response_model = resp.model.clone();
        last_raw_text       = resp.text.clone();
        if let Some(n) = resp.tokens_in {
            tokens_in_total = Some(tokens_in_total.unwrap_or(0) + n);
        }
        if let Some(n) = resp.tokens_out {
            tokens_out_total = Some(tokens_out_total.unwrap_or(0) + n);
        }

        let script = extract_bund_block(&resp.text);
        last_script = script.clone();

        let parse_result = bund_parse(&format!("{script}\n"));
        let parse_failure: Option<String> = match parse_result {
            Ok(ast) => {
                // Parse succeeded — now check that every word reference
                // is actually a registered VM word.  An "undefined word"
                // is just as broken as a syntax error from the caller's
                // perspective, so we feed it back through the same
                // retry loop.
                let unknown = undefined_words(&ast, &known_words);
                if unknown.is_empty() {
                    let ms = started.elapsed().as_millis() as u64;
                    return Ok(Translation {
                        script,
                        valid:          true,
                        parse_attempts: attempt + 1,
                        parse_error:    None,
                        provider:       provider.id().to_owned(),
                        model:          last_response_model,
                        ms,
                        tokens_in:      tokens_in_total,
                        tokens_out:     tokens_out_total,
                    });
                }
                Some(format_unknown_words_error(&unknown))
            }
            Err(e) => Some(format!("{e}")),
        };

        let emsg = parse_failure.expect("non-Ok branch sets parse_failure");
        last_parse_err = emsg.clone();
        if attempt == max_retries { break; }

        // Append the model's output + a retry instruction.  The error
        // text already explains whether the failure was a syntax error
        // or an undefined-word reference; we just ask the model to fix
        // it.
        messages.push(Message::assistant(&resp.text));
        messages.push(Message {
            role:    Role::User,
            content: format!(
                "Your previous output failed to validate with this error:\n\n\
                 {emsg}\n\n\
                 Fix the issue and emit the corrected script.  Do not \
                 change the script's intent.  Reply with a single \
                 fenced ```bund …``` block and nothing else."
            ),
        });
    }

    let ms = started.elapsed().as_millis() as u64;
    let script = if last_script.trim().is_empty() { last_raw_text } else { last_script };
    Ok(Translation {
        script,
        valid:          false,
        parse_attempts: max_retries + 1,
        parse_error:    Some(last_parse_err),
        provider:       provider.id().to_owned(),
        model:          last_response_model,
        ms,
        tokens_in:      tokens_in_total,
        tokens_out:     tokens_out_total,
    })
}

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

/// Walk a parsed Bund AST and return the alphabetically-sorted set of
/// word names that are not present in `known`.  Returns `vec![]` when
/// `known` is empty (Adam not initialised / no policy loaded) so the
/// translator degrades gracefully in library-without-bdsnode use.
///
/// The walk descends into nested `LIST`, `LAMBDA`, `CONTEXT`, `MAP`,
/// and `CALL` attr-vectors so an undefined word buried inside a
/// `{ … }` body still surfaces.
pub fn undefined_words(ast: &[DynValue], known: &BTreeSet<String>) -> Vec<String> {
    if known.is_empty() { return Vec::new(); }
    let mut found: BTreeSet<String> = BTreeSet::new();
    for v in ast {
        collect_unknown_calls(v, known, &mut found);
    }
    found.into_iter().collect()
}

fn collect_unknown_calls(v: &DynValue, known: &BTreeSet<String>, out: &mut BTreeSet<String>) {
    if v.dt == CALL {
        if let Val::String(name) = &v.data {
            let trimmed = name.trim();
            if !trimmed.is_empty() && !known.contains(trimmed) {
                out.insert(trimmed.to_owned());
            }
        }
    }
    // The parser stores lambda / list / map / queue / matrix bodies in
    // `data`, not `attr`, so we have to walk the Val variant.  `attr`
    // is empty for parser-emitted values but the recursion is cheap
    // and future-proof.
    match &v.data {
        Val::List(inner) | Val::Lambda(inner) | Val::Queue(inner) => {
            for child in inner { collect_unknown_calls(child, known, out); }
        }
        Val::Matrix(rows) => {
            for row in rows {
                for child in row { collect_unknown_calls(child, known, out); }
            }
        }
        Val::Map(m) => {
            for child in m.values() { collect_unknown_calls(child, known, out); }
        }
        Val::ValueMap(m) => {
            for (k, vv) in m {
                collect_unknown_calls(k,  known, out);
                collect_unknown_calls(vv, known, out);
            }
        }
        _ => {}
    }
    for child in &v.attr {
        collect_unknown_calls(child, known, out);
    }
}

/// Render a friendly retry instruction listing the unknown words the
/// model emitted.  Truncates long lists so the retry user-turn stays
/// readable even when the model spews dozens of fake names.
fn format_unknown_words_error(unknown: &[String]) -> String {
    const MAX_SHOWN: usize = 12;
    let total = unknown.len();
    let head: Vec<&str> = unknown.iter().take(MAX_SHOWN).map(|s| s.as_str()).collect();
    let suffix = if total > MAX_SHOWN {
        format!(" (and {} more)", total - MAX_SHOWN)
    } else {
        String::new()
    };
    format!(
        "The script references {total} word(s) that are not registered \
         in this Bund VM: {}{suffix}.  Use only words listed in the \
         stdlib catalogue from the system prompt, or define new words \
         inline via `:name {{ body }} register`.",
        head.join(", "),
    )
}

/// Extract a `bund` code block from an LLM response.  Looks for, in
/// order: a ```bund-tagged fence, any triple-backtick fence, the
/// whole response stripped.  The model is instructed to emit exactly
/// one fence, but real-world output sometimes leaks prose around the
/// fence so the extractor is forgiving.
pub fn extract_bund_block(text: &str) -> String {
    // 1) Look for explicit ```bund block (case-insensitive).
    if let Some(s) = find_fenced(text, Some("bund")) {
        return s;
    }
    // 2) Any ``` block (often the model omits the language tag).
    if let Some(s) = find_fenced(text, None) {
        return s;
    }
    // 3) Fallback — strip leading/trailing whitespace, assume the
    //    whole response is the script.
    text.trim().to_owned()
}

/// Return the contents of the first fenced block, optionally
/// requiring a specific language tag.  Comparison on the tag is
/// case-insensitive.
fn find_fenced(text: &str, want_tag: Option<&str>) -> Option<String> {
    let mut iter = text.lines();
    while let Some(line) = iter.next() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("```") { continue; }
        let tag = trimmed.trim_start_matches('`').trim();
        if let Some(want) = want_tag {
            if !tag.eq_ignore_ascii_case(want) { continue; }
        }
        // Body — collect until we hit the closing fence.
        let mut body = String::new();
        for inner in iter.by_ref() {
            if inner.trim_start().starts_with("```") {
                return Some(body.trim_end().to_owned());
            }
            body.push_str(inner);
            body.push('\n');
        }
        // No closing fence — take everything we have.
        return Some(body.trim_end().to_owned());
    }
    None
}

/// Pull standard [`CompletionOpts`] knobs from a JSON object.
/// Anything not present is left at the default.
fn parse_options(req: &JsonValue) -> CompletionOpts {
    let opts = req.get("options").and_then(|v| v.as_object());
    let mut out = CompletionOpts::default();
    if let Some(o) = opts {
        out.temperature = o.get("temperature").and_then(|v| v.as_f64()).map(|f| f as f32);
        out.max_tokens  = o.get("max_tokens").and_then(|v| v.as_u64()).map(|n| n as u32);
        out.top_p       = o.get("top_p").and_then(|v| v.as_f64()).map(|f| f as f32);
        out.seed        = o.get("seed").and_then(|v| v.as_u64());
        out.num_ctx     = o.get("num_ctx").and_then(|v| v.as_u64()).map(|n| n as u32);
        if let Some(arr) = o.get("stop").and_then(|v| v.as_array()) {
            out.stop = arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        }
    }
    out
}

/// Serialise a [`Translation`] for the JSON-RPC response envelope.
pub fn translation_to_json(t: &Translation) -> JsonValue {
    let mut o = json!({
        "script":         t.script,
        "valid":          t.valid,
        "parse_attempts": t.parse_attempts,
        "provider":       t.provider,
        "model":          t.model,
        "ms":             t.ms,
    });
    if let Some(ref e) = t.parse_error {
        o["parse_error"] = json!(e);
    }
    if let Some(n) = t.tokens_in  { o["tokens_in"]  = json!(n); }
    if let Some(n) = t.tokens_out { o["tokens_out"] = json!(n); }
    o
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests — pure functions only (no LLM provider)
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_handles_tagged_fence() {
        let s = "some prose\n```bund\n1 2 + println\n```\ntrailing";
        assert_eq!(extract_bund_block(s), "1 2 + println");
    }

    #[test]
    fn extract_handles_untagged_fence() {
        let s = "blah\n```\n1 2 + println\n```\n";
        assert_eq!(extract_bund_block(s), "1 2 + println");
    }

    #[test]
    fn extract_case_insensitive_tag() {
        let s = "```BUND\n42 println\n```";
        assert_eq!(extract_bund_block(s), "42 println");
    }

    #[test]
    fn extract_multiline_body() {
        let s = "```bund\n// hello\n1 2 +\nprintln\n```";
        assert_eq!(extract_bund_block(s), "// hello\n1 2 +\nprintln");
    }

    #[test]
    fn extract_no_fence_returns_trimmed_input() {
        // Real-world models occasionally drop the fence; the
        // extractor falls back to the raw text so parse_validate
        // still gets a shot at it.
        let s = "  1 2 + println  ";
        assert_eq!(extract_bund_block(s), "1 2 + println");
    }

    #[test]
    fn extract_unterminated_fence_takes_rest() {
        let s = "```bund\n1 2 + println";
        assert_eq!(extract_bund_block(s), "1 2 + println");
    }

    #[test]
    fn extract_prefers_bund_tag_over_other_tags() {
        let s = "```python\nprint(1)\n```\nand then:\n```bund\n1 println\n```";
        assert_eq!(extract_bund_block(s), "1 println");
    }

    #[test]
    fn parse_options_reads_full_block() {
        let req = json!({"options": {
            "temperature": 0.4, "max_tokens": 200, "top_p": 0.9,
            "seed": 42, "num_ctx": 32768, "stop": ["STOP"]
        }});
        let o = parse_options(&req);
        assert_eq!(o.temperature, Some(0.4));
        assert_eq!(o.max_tokens,  Some(200));
        assert_eq!(o.top_p,       Some(0.9));
        assert_eq!(o.seed,        Some(42));
        assert_eq!(o.num_ctx,     Some(32768));
        assert_eq!(o.stop,        vec!["STOP".to_string()]);
    }

    #[test]
    fn translation_to_json_omits_optional_fields() {
        let t = Translation {
            script: "1 println".into(), valid: true, parse_attempts: 1,
            parse_error: None, provider: "ollama".into(), model: "llama3.2".into(),
            ms: 12, tokens_in: None, tokens_out: None,
        };
        let j = translation_to_json(&t);
        assert!(j.get("parse_error").is_none());
        assert!(j.get("tokens_in").is_none());
        assert!(j.get("tokens_out").is_none());
        assert_eq!(j["valid"], json!(true));
    }

    #[test]
    fn undefined_words_returns_empty_when_known_set_is_empty() {
        let ast = bund_parse("nonexistent.word\n").unwrap();
        // Empty known set → degrade gracefully (no false positives).
        assert!(undefined_words(&ast, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn undefined_words_flags_unknown_top_level_calls() {
        let ast = bund_parse("1 2 +\nfoo.bar\nprintln\n").unwrap();
        let known: BTreeSet<String> =
            ["+".to_string(), "println".to_string()].into_iter().collect();
        let missing = undefined_words(&ast, &known);
        assert_eq!(missing, vec!["foo.bar".to_string()]);
    }

    #[test]
    fn undefined_words_recurses_into_lambdas_and_lists() {
        // The undefined `make.unicorn` is nested inside a lambda body
        // inside a list — walk must find it.
        let ast = bund_parse("[ 1 2 { make.unicorn } ]\n").unwrap();
        let known: BTreeSet<String> = ["+".to_string(), "println".to_string()]
            .into_iter().collect();
        let missing = undefined_words(&ast, &known);
        assert!(missing.iter().any(|s| s == "make.unicorn"),
            "expected nested undefined word to surface, got: {missing:?}");
    }

    #[test]
    fn undefined_words_dedup_and_sort() {
        let ast = bund_parse("baz foo bar foo baz\n").unwrap();
        // Non-empty known set that excludes all three — each
        // duplicate should collapse and the output should be sorted.
        let known: BTreeSet<String> = ["+".to_string()].into_iter().collect();
        let missing = undefined_words(&ast, &known);
        assert_eq!(missing, vec!["bar".to_string(), "baz".to_string(), "foo".to_string()]);
    }

    #[test]
    fn format_unknown_words_error_truncates_long_lists() {
        let many: Vec<String> = (0..20).map(|i| format!("w{i}")).collect();
        let msg = format_unknown_words_error(&many);
        assert!(msg.contains("references 20 word(s)"));
        assert!(msg.contains("and 8 more"));
        // First 12 words present, last few elided.
        assert!(msg.contains("w0"));
        assert!(msg.contains("w11"));
        assert!(!msg.contains("w12,"));
    }

    #[test]
    fn translation_to_json_includes_error_when_invalid() {
        let t = Translation {
            script: "1 2 +++".into(), valid: false, parse_attempts: 3,
            parse_error: Some("Error parsing token: …".into()),
            provider: "ollama".into(), model: "llama3.2".into(),
            ms: 4012, tokens_in: Some(100), tokens_out: Some(20),
        };
        let j = translation_to_json(&t);
        assert_eq!(j["valid"], json!(false));
        assert_eq!(j["parse_error"], json!("Error parsing token: …"));
        assert_eq!(j["tokens_in"], json!(100));
    }
}
