//! `v3/help` — docstore-backed Q&A helper.
//!
//! Takes a natural-language `message`, retrieves the top-`limit`
//! matching documents from the cluster docstore (which is fully
//! replicated across every peer, so a local `doc_search_text` covers
//! the whole cluster), optionally filters to internal-only documents
//! (`metadata.internal_doc == true` — the tag emitted by
//! `scripts/load_internal_documentation.sh`), packs them into a RAG
//! prompt, and runs one completion against the configured default
//! LLM provider.
//!
//! Unlike `v4/llm.analyze` with `kind=documents`, this surface:
//!
//! - **Searches** the docstore for relevant documents (analyze takes
//!   pre-resolved ids).
//! - Has a **first-class `internal_only` switch** so consumers can
//!   restrict answers to the operator-curated corpus loaded via
//!   `load_internal_documentation.sh`.
//! - Returns a **sources[]** list (id, name, score, internal_doc)
//!   alongside the answer so the caller can show citations.
//!
//! It deliberately does NOT cache, does NOT replicate (read-only),
//! and does NOT execute anything — purely retrieval + completion.

use crate::common::error::{err_msg, Result};
use crate::globals::get_db;
use crate::llm::manager;
use crate::llm::types::{CompletionOpts, CompletionRequest, Message};
use crate::vm::api::runtime;
use serde_json::{json, Value as JsonValue};
use std::time::Instant;

// ─────────────────────────────────────────────────────────────────────
// Tunable constants — sane defaults that hit the prompt-budget sweet
// spot.  Move to `llm.help.*` in bds.hjson if operators want to
// override.
// ─────────────────────────────────────────────────────────────────────

/// Default number of docs to include in the RAG context when the
/// caller doesn't pin `limit`.  8 is a good balance between coverage
/// and prompt size for a 32 k-token context.
pub const DEFAULT_LIMIT: usize = 8;

/// Hard ceiling on `limit` so a runaway call can't drown the model
/// in a hundred docs at once.
pub const MAX_LIMIT: usize = 50;

/// Per-document content truncation when packing into the prompt.
/// Whole-document RAG works on long markdown files but only if we
/// keep each entry to a few thousand chars — the embedder produces
/// one vector per doc regardless of length, but the prompt has to
/// fit.  Operators with bigger context windows can override per-call
/// via `options.num_ctx` and the script itself can stretch by
/// chunking via `doc-add-file` instead.
const MAX_CONTENT_CHARS: usize = 8_000;

/// Trailing ellipsis appended when a doc's content is truncated.
/// Kept short so it doesn't burn tokens.
const TRUNC_MARKER: &str = "\n\n[…content truncated]\n";

/// Baked system prompt.  Tight, grounded, refusal-friendly — designed
/// for ops Q&A over a curated doc base rather than open-ended chat.
const SYSTEM_PROMPT: &str = "You are a helpful assistant answering operator questions \
strictly from the supplied documents.\n\
\n\
Rules:\n\
- Use ONLY the document content provided in this turn.  Do not invent \
  facts, words, or APIs that are not in the documents.\n\
- When you cite something, mention the document name in square brackets, \
  e.g. \"[BDSCONFIG.md]\".\n\
- If the documents do not contain the answer, say so plainly — do not \
  guess from general knowledge.\n\
- Keep answers concise: a few sentences when possible, bullet points or \
  short code blocks when the question is procedural.\n\
- Quote exact command lines, paths, or config keys verbatim from the \
  documents whenever they appear in the answer.\n";

// ─────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────

/// One call's worth of output.  Mirrors the `Translation` struct in
/// `to_bund` — kept narrow so JSON-RPC handlers don't have to bother
/// with optional-field gymnastics.
#[derive(Debug, Clone)]
pub struct HelpResponse {
    /// LLM answer body (already stripped of trailing whitespace).
    pub answer:        String,
    /// Number of documents that ended up in the prompt after filter
    /// + truncation.  Zero when the search returned nothing.
    pub n_docs:        usize,
    /// What the caller asked for so consumers can render "you asked
    /// for internal_only=true and got 4 / 8 docs".
    pub internal_only: bool,
    pub limit:         usize,
    /// One entry per cited document, in score order (descending).
    pub sources:       Vec<HelpSource>,
    pub provider:      String,
    pub model:         String,
    pub ms:            u64,
    pub tokens_in:     Option<u32>,
    pub tokens_out:    Option<u32>,
    /// Empty when the search returned at least one document; set to
    /// a short human-readable string when no docs matched (the model
    /// is still called so the answer is consistent).
    pub note:          String,
}

#[derive(Debug, Clone)]
pub struct HelpSource {
    pub id:           String,
    /// `metadata.name` if present, else `metadata.path`, else `"<no name>"`.
    pub name:         String,
    pub score:        f32,
    pub internal_doc: bool,
}

/// Per-call overrides shared with the JSON-RPC handler.  Everything
/// is optional; the helper falls back to provider / model defaults
/// when fields are empty.
#[derive(Debug, Clone, Default)]
pub struct HelpRequest {
    pub message:       String,
    pub internal_only: bool,
    pub limit:         Option<usize>,
    pub provider:      Option<String>,
    pub model:         Option<String>,
    pub options:       CompletionOpts,
}

/// Run one Q&A turn against the docstore + default LLM provider.
///
/// On success the response is structured (see [`HelpResponse`]).  On
/// LLM / provider failure an `Err` is returned with the upstream
/// error message — callers should surface it as RPC error `-32004`.
pub fn help(req: HelpRequest) -> Result<HelpResponse> {
    let message = req.message.trim();
    if message.is_empty() {
        return Err(err_msg("v3/help: `message` is required and must be non-empty"));
    }

    // Clamp limit to a sane window.  `0` is allowed (returns whatever
    // the model knows without RAG) but we coerce to 1 to keep the
    // search side happy.
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    // Provider resolution.  `""`/None → manager's default.
    let mgr = manager::manager()
        .ok_or_else(|| err_msg("v3/help: provider manager not initialised"))?;
    let resolver = req.provider.as_deref().filter(|s| !s.is_empty());
    let provider = mgr.resolve(resolver)
        .map_err(|e| err_msg(format!("v3/help: {e}")))?;
    let model = match req.model.as_deref() {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => provider.default_model().to_owned(),
    };

    // Document retrieval.  The docstore is one of the fully-replicated
    // cluster stores (alongside signals/scripts/users/llm_cache), so a
    // local search already covers every peer's data.
    let raw_hits = docstore_search(message, limit, req.internal_only)?;
    let sources: Vec<HelpSource> = raw_hits.iter().map(hit_to_source).collect();

    // Assemble the prompt.  Empty hits → the system prompt still
    // tells the model to refuse, and the user-turn carries an
    // explicit "no documents matched" note so the answer is
    // consistent.
    let context_block = build_context_block(&raw_hits);
    let user_content = if raw_hits.is_empty() {
        format!(
            "No documents matched the question.  Answer the user's \
             request below; if you cannot answer without source \
             material, say so plainly.\n\n\
             User question: {message}"
        )
    } else {
        format!(
            "Use the following documents to answer the user's \
             question.  Cite document names in square brackets.\n\n\
             {context_block}\n\nUser question: {message}"
        )
    };

    let messages = vec![
        Message::system(SYSTEM_PROMPT),
        Message::user(&user_content),
    ];

    // Bump num_ctx based on prompt size if the caller didn't pin it
    // (Ollama defaults to 2048 which is laughable for whole-doc RAG).
    let mut options = req.options;
    if options.num_ctx.is_none() {
        let approx_tokens = (SYSTEM_PROMPT.len() + user_content.len()) / 4 + 1024;
        options.num_ctx = Some(match approx_tokens {
            0..=8192      => 16384,
            8193..=16384  => 32768,
            _             => 65536,
        });
    }

    let started = Instant::now();
    let rq = CompletionRequest { model: model.clone(), messages, options };
    let resp = runtime::block_on(provider.complete(rq))
        .map_err(|e| err_msg(format!("v3/help: provider {:?}: {e}", provider.id())))?;
    let ms = started.elapsed().as_millis() as u64;

    let note = if raw_hits.is_empty() {
        let scope = if req.internal_only { "internal" } else { "any" };
        format!("no {scope} documents matched — answer based on the model's general knowledge")
    } else {
        String::new()
    };

    Ok(HelpResponse {
        answer:        resp.text.trim().to_owned(),
        n_docs:        raw_hits.len(),
        internal_only: req.internal_only,
        limit,
        sources,
        provider:      provider.id().to_owned(),
        model:         resp.model,
        ms,
        tokens_in:     resp.tokens_in,
        tokens_out:    resp.tokens_out,
        note,
    })
}

/// Serialise a [`HelpResponse`] for the JSON-RPC envelope.  Omits
/// optional fields when they're absent so downstream consumers can
/// branch on presence cleanly.
pub fn response_to_json(r: &HelpResponse) -> JsonValue {
    let mut o = json!({
        "answer":         r.answer,
        "n_docs":         r.n_docs,
        "internal_only":  r.internal_only,
        "limit":          r.limit,
        "provider":       r.provider,
        "model":          r.model,
        "ms":             r.ms,
        "sources":        r.sources.iter().map(|s| json!({
            "id":           s.id,
            "name":         s.name,
            "score":        s.score,
            "internal_doc": s.internal_doc,
        })).collect::<Vec<_>>(),
    });
    if let Some(n) = r.tokens_in  { o["tokens_in"]  = json!(n); }
    if let Some(n) = r.tokens_out { o["tokens_out"] = json!(n); }
    if !r.note.is_empty()         { o["note"]       = json!(r.note); }
    o
}

// ─────────────────────────────────────────────────────────────────────
// Internals
// ─────────────────────────────────────────────────────────────────────

/// Run the docstore search, optionally filter to internal-only docs,
/// and truncate to `limit`.  Fetches `4 × limit` candidates when
/// `internal_only` so post-hoc filtering still has enough material to
/// fill the slot count.
fn docstore_search(query: &str, limit: usize, internal_only: bool) -> Result<Vec<JsonValue>> {
    let db = get_db()
        .map_err(|e| err_msg(format!("v3/help: get_db: {e}")))?;
    let fetch = if internal_only { limit.saturating_mul(4).max(limit) } else { limit };
    let mut hits = db.doc_search_text(query, fetch)
        .map_err(|e| err_msg(format!("v3/help: doc_search_text: {e}")))?;

    if internal_only {
        hits.retain(|h| is_internal_doc(h));
    }
    hits.truncate(limit);
    Ok(hits)
}

fn is_internal_doc(hit: &JsonValue) -> bool {
    hit.get("metadata")
        .and_then(|m| m.get("internal_doc"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn hit_to_source(hit: &JsonValue) -> HelpSource {
    let id = hit.get("id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let metadata = hit.get("metadata");
    let name = metadata
        .and_then(|m| m.get("name").and_then(|v| v.as_str()))
        .or_else(|| metadata.and_then(|m| m.get("path").and_then(|v| v.as_str())))
        .unwrap_or("<no name>")
        .to_owned();
    let score = hit.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
    HelpSource { id, name, score, internal_doc: is_internal_doc(hit) }
}

/// Build the `[document N — name] …content…` block that gets spliced
/// into the user turn.  Each doc is truncated to `MAX_CONTENT_CHARS`
/// so a single 150-KB markdown file can't blow the prompt budget.
fn build_context_block(hits: &[JsonValue]) -> String {
    let mut out = String::new();
    for (i, hit) in hits.iter().enumerate() {
        let source = hit_to_source(hit);
        let content = hit.get("document").and_then(|v| v.as_str()).unwrap_or("");
        let trimmed = truncate_content(content);
        out.push_str(&format!(
            "[document {idx} — {name}]\n{body}\n\n",
            idx  = i + 1,
            name = source.name,
            body = trimmed,
        ));
    }
    out.trim_end().to_owned()
}

fn truncate_content(s: &str) -> String {
    if s.len() <= MAX_CONTENT_CHARS {
        return s.to_owned();
    }
    // Cut at a UTF-8 character boundary at or below the cap.
    let mut end = MAX_CONTENT_CHARS;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    let mut out = String::with_capacity(end + TRUNC_MARKER.len());
    out.push_str(&s[..end]);
    out.push_str(TRUNC_MARKER);
    out
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests — pure helpers only
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_hit(id: &str, name: &str, score: f64, internal: Option<bool>, body: &str) -> JsonValue {
        let mut metadata = serde_json::Map::new();
        metadata.insert("name".into(), json!(name));
        if let Some(b) = internal {
            metadata.insert("internal_doc".into(), json!(b));
        }
        json!({
            "id":       id,
            "metadata": JsonValue::Object(metadata),
            "document": body,
            "score":    score,
        })
    }

    #[test]
    fn is_internal_doc_handles_missing_and_false() {
        let with_true   = mk_hit("a", "a.md", 0.9, Some(true),  "x");
        let with_false  = mk_hit("b", "b.md", 0.8, Some(false), "x");
        let without     = mk_hit("c", "c.md", 0.7, None,        "x");
        assert!( is_internal_doc(&with_true));
        assert!(!is_internal_doc(&with_false));
        assert!(!is_internal_doc(&without));
    }

    #[test]
    fn hit_to_source_falls_back_through_name_path_placeholder() {
        // name present.
        let h1 = mk_hit("aaa", "README.md", 0.5, None, "");
        assert_eq!(hit_to_source(&h1).name, "README.md");

        // no name, path present.
        let h2 = json!({
            "id":       "bbb",
            "metadata": {"path": "doc/sub.md"},
            "score":    0.4,
            "document": "",
        });
        assert_eq!(hit_to_source(&h2).name, "doc/sub.md");

        // neither.
        let h3 = json!({"id":"ccc","metadata":{},"score":0.3,"document":""});
        assert_eq!(hit_to_source(&h3).name, "<no name>");
    }

    #[test]
    fn truncate_content_passes_short_strings_through() {
        let s = "hello world";
        assert_eq!(truncate_content(s), s);
    }

    #[test]
    fn truncate_content_caps_long_input_and_appends_marker() {
        let big = "a".repeat(MAX_CONTENT_CHARS * 2);
        let out = truncate_content(&big);
        assert!(out.starts_with(&"a".repeat(MAX_CONTENT_CHARS)));
        assert!(out.ends_with(TRUNC_MARKER));
        assert!(out.len() <= MAX_CONTENT_CHARS + TRUNC_MARKER.len());
    }

    #[test]
    fn truncate_content_does_not_split_utf8_codepoints() {
        // Build a string of multi-byte chars longer than the cap.
        let unit  = "üü";                                  // 4 bytes
        let count = MAX_CONTENT_CHARS / unit.len() + 5;    // overshoot
        let big   = unit.repeat(count);
        let out   = truncate_content(&big);
        // If we split mid-codepoint, .chars() would fail to decode —
        // count it instead to assert decoding is clean.
        let _: usize = out.chars().count();
    }

    #[test]
    fn build_context_block_labels_each_doc() {
        let hits = vec![
            mk_hit("a","first.md",  0.9, Some(true), "alpha body"),
            mk_hit("b","second.md", 0.7, None,        "beta body"),
        ];
        let block = build_context_block(&hits);
        assert!(block.contains("[document 1 — first.md]"));
        assert!(block.contains("alpha body"));
        assert!(block.contains("[document 2 — second.md]"));
        assert!(block.contains("beta body"));
    }

    #[test]
    fn build_context_block_empty_input_returns_empty_string() {
        assert_eq!(build_context_block(&[]), "");
    }

    #[test]
    fn response_to_json_omits_unset_optional_fields() {
        let r = HelpResponse {
            answer: "ok".into(), n_docs: 0, internal_only: false, limit: 8,
            sources: vec![], provider: "ollama".into(), model: "llama3.2".into(),
            ms: 10, tokens_in: None, tokens_out: None, note: "".into(),
        };
        let j = response_to_json(&r);
        assert!(j.get("tokens_in").is_none());
        assert!(j.get("tokens_out").is_none());
        assert!(j.get("note").is_none());
        assert_eq!(j["limit"], json!(8));
        assert_eq!(j["sources"], json!([]));
    }

    #[test]
    fn response_to_json_emits_note_and_tokens_when_present() {
        let r = HelpResponse {
            answer: "see [README.md]".into(), n_docs: 1, internal_only: true, limit: 8,
            sources: vec![HelpSource {
                id: "x".into(), name: "README.md".into(),
                score: 0.5, internal_doc: true,
            }],
            provider: "ollama".into(), model: "llama3.2".into(),
            ms: 12, tokens_in: Some(100), tokens_out: Some(20),
            note: "demo".into(),
        };
        let j = response_to_json(&r);
        assert_eq!(j["tokens_in"],  json!(100));
        assert_eq!(j["tokens_out"], json!(20));
        assert_eq!(j["note"],       json!("demo"));
        assert_eq!(j["sources"][0]["internal_doc"], json!(true));
    }
}
