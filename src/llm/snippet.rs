//! Detect Bund snippets inside a chat-message body.
//!
//! Two syntaxes (in priority order):
//!
//! 1. **Fenced code block** — Markdown-style ` ```bund … ``` ` anywhere
//!    in the message.  Wins over the leading-`/` form when both are
//!    present.
//! 2. **Leading `/`** — line that starts with `/`, immediately followed
//!    by a Bund-looking token starter (configurable strictness).  The
//!    rest of that line is the code body; anything after a blank line
//!    becomes the natural-language remainder.
//!
//! The detector is pure parser — no Bund VM, no eval, no I/O.  Phase 1
//! plugs it into `vm::api::llm::chat` and runs the resulting `code`
//! through a worker thread with a tokio timeout.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnippetSource {
    /// ```bund … ``` block.
    Fenced,
    /// Single line starting with `/`.
    LeadingSlash,
}

impl SnippetSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SnippetSource::Fenced       => "fenced",
            SnippetSource::LeadingSlash => "slash",
        }
    }
}

/// Output of [`extract_bund_snippet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundSnippet {
    pub source:    SnippetSource,
    /// The Bund code body (no fence markers, no leading `/`).
    pub code:      String,
    /// Whatever text wasn't part of the snippet.  Becomes the natural-
    /// language remainder fed to the LLM as the question after the `---`
    /// separator that the chat prompt assembly already uses.  Trimmed
    /// of leading/trailing whitespace.  May be empty.
    pub remainder: String,
}

/// Strictness of the leading-`/` detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashStrictness {
    /// Require the byte AFTER `/` to be a Bund-looking token starter:
    /// ASCII letter / digit / `$` / `"`.  Rejects path-like inputs
    /// such as `/etc/hosts` or `/api/v1/items/42`.
    Strict,
    /// Anything after the `/` goes to the parser.  Useful when the
    /// operator has trained their users on the slash form and accepts
    /// the false-positive risk.
    Permissive,
}

impl SlashStrictness {
    pub fn from_wire(s: &str) -> Self {
        match s {
            "permissive" => Self::Permissive,
            _            => Self::Strict,
        }
    }
}

/// Detector options.  Mirrors `llm.chat.bund.*` in bds.hjson.
#[derive(Debug, Clone, Copy)]
pub struct DetectOpts {
    pub fenced_only:      bool,
    pub slash_strictness: SlashStrictness,
}

impl Default for DetectOpts {
    fn default() -> Self {
        Self {
            fenced_only:      false,
            slash_strictness: SlashStrictness::Strict,
        }
    }
}

/// Return the first matching Bund snippet in `message`, or `None`.
pub fn extract_bund_snippet(message: &str, opts: DetectOpts) -> Option<BundSnippet> {
    if let Some(s) = extract_fenced(message) {
        return Some(s);
    }
    if opts.fenced_only {
        return None;
    }
    extract_leading_slash(message, opts.slash_strictness)
}

// ─────────────────────────────────────────────────────────────────────
// Fenced
// ─────────────────────────────────────────────────────────────────────

/// Match ` ```bund ` … ` ``` ` (case-insensitive on the language tag).
/// The fence MUST be `bund` — generic ` ``` ` blocks are NOT picked up
/// (they're typically operator output the user is sharing with the
/// model, not code they want executed).
fn extract_fenced(message: &str) -> Option<BundSnippet> {
    // Cheap pre-check.
    if !message.contains("```") { return None; }

    let lower = message.to_ascii_lowercase();
    // Find a ```bund opening fence on its own line (or at offset 0).
    let mut search_from = 0usize;
    let open_idx = loop {
        let candidate = lower[search_from..].find("```bund")?;
        let abs = search_from + candidate;
        // Must be at the start of the message OR preceded by a newline.
        if abs == 0 || message.as_bytes().get(abs - 1) == Some(&b'\n') {
            break abs;
        }
        search_from = abs + 1;
    };

    // The opening fence runs `\`\`\`bund` (7 bytes) followed by an
    // optional newline / whitespace.  Code starts after the newline.
    let after_fence = open_idx + "```bund".len();
    // Skip the rest of the fence-line (any trailing chars on the
    // ```bund line, e.g. an attribute marker — uncommon but valid).
    let body_start = match message[after_fence..].find('\n') {
        Some(off) => after_fence + off + 1,
        None      => return None,   // no newline after ```bund → malformed
    };

    // Closing fence: first ``` that sits at the start of its own line.
    let body_section = &message[body_start..];
    let mut close_search_from = 0usize;
    let close_offset = loop {
        let candidate = body_section[close_search_from..].find("```")?;
        let abs = close_search_from + candidate;
        if abs == 0 || body_section.as_bytes().get(abs - 1) == Some(&b'\n') {
            break abs;
        }
        close_search_from = abs + 3;
    };

    let code = body_section[..close_offset].trim_end_matches('\n').to_owned();
    // Bytes after the closing ``` line.
    let close_end = body_start + close_offset + 3;
    let tail_start = match message[close_end..].find('\n') {
        Some(off) => close_end + off + 1,
        None      => message.len(),  // closing fence is final line
    };

    // Remainder = everything before the opening fence + everything
    // after the closing fence, joined by a single newline if both
    // parts are non-empty.
    let before  = message[..open_idx].trim();
    let after   = message[tail_start..].trim();
    let remainder = match (before.is_empty(), after.is_empty()) {
        (true,  true)  => String::new(),
        (false, true)  => before.to_owned(),
        (true,  false) => after.to_owned(),
        (false, false) => format!("{before}\n{after}"),
    };

    if code.trim().is_empty() {
        return None;  // ```bund\n``` with no body → not a snippet
    }

    Some(BundSnippet {
        source: SnippetSource::Fenced,
        code,
        remainder,
    })
}

// ─────────────────────────────────────────────────────────────────────
// Leading-slash
// ─────────────────────────────────────────────────────────────────────

/// Match a non-empty first line starting with `/`.  Strict mode also
/// requires the byte after `/` to look like a Bund token (letter / digit
/// / `$` / `"`) so `/etc/hosts` and `/api/v1/items/42` don't get parsed
/// as code.
fn extract_leading_slash(message: &str, strictness: SlashStrictness) -> Option<BundSnippet> {
    let trimmed = message.trim_start_matches([' ', '\t']);
    let bytes = trimmed.as_bytes();
    if bytes.first().copied() != Some(b'/') {
        return None;
    }
    // Reject `//` (could be a path or a comment) — comments aren't a
    // Bund execution intent.
    if bytes.get(1).copied() == Some(b'/') {
        return None;
    }

    if matches!(strictness, SlashStrictness::Strict) {
        match bytes.get(1).copied() {
            None => return None,                        // bare "/" — empty
            Some(b) if b.is_ascii_alphanumeric()
                    || b == b'$' || b == b'"' || b == b'(' => {},
            _ => return None,                           // not a Bund token start
        }
        // Path-like reject: if the first whitespace-bounded token
        // contains another `/`, it's almost certainly a path
        // (`/etc/hosts`, `/api/v1/items/42`) and not a Bund snippet.
        // Bund words use `.` as the separator (`cls.knn`); they don't
        // contain `/`.
        let first_word_end = trimmed[1..]
            .find(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(trimmed.len());
        if trimmed[1..first_word_end].contains('/') {
            return None;
        }
    }

    // Split on the first blank line — anything after that is the
    // natural-language remainder.  Without a blank line, the slash
    // line is the whole snippet and the remainder is empty.
    let (snippet_lines, remainder) = match find_blank_line_split(trimmed) {
        Some((s, r)) => (s, r),
        None         => (trimmed, ""),
    };

    // Take just the first contiguous block — every line up to the next
    // line that does NOT start with `/`.  Multi-line slash snippets
    // are supported: `/foo\n/bar\n/baz` becomes a 3-line body.
    let mut code_lines: Vec<&str> = Vec::new();
    let mut consumed_chars = 0usize;
    for line in snippet_lines.lines() {
        if line.starts_with('/') {
            code_lines.push(&line[1..]);     // drop the leading `/`
            consumed_chars += line.len() + 1;  // +1 for the newline
        } else {
            break;
        }
    }
    if code_lines.is_empty() {
        return None;
    }

    // Anything left in `snippet_lines` past `consumed_chars` is the
    // tail of the same block — fold it into the remainder so the model
    // still sees it.
    let untaken_tail = if consumed_chars <= snippet_lines.len() {
        snippet_lines[consumed_chars..].trim()
    } else {
        ""
    };
    let full_remainder = match (untaken_tail.is_empty(), remainder.is_empty()) {
        (true,  true)  => String::new(),
        (false, true)  => untaken_tail.to_owned(),
        (true,  false) => remainder.trim().to_owned(),
        (false, false) => format!("{untaken_tail}\n\n{}", remainder.trim()),
    };

    let code = code_lines.join("\n").trim().to_owned();
    if code.is_empty() {
        return None;
    }

    Some(BundSnippet {
        source: SnippetSource::LeadingSlash,
        code,
        remainder: full_remainder,
    })
}

/// Return `Some((before_blank, after_blank))` if `s` contains an empty
/// or whitespace-only line.  The split is exclusive of the blank line
/// itself.
fn find_blank_line_split(s: &str) -> Option<(&str, &str)> {
    let mut pos = 0usize;
    for line in s.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if bare.is_empty() || bare.chars().all(char::is_whitespace) {
            return Some((&s[..pos], &s[pos + line.len()..]));
        }
        pos += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(s: &str) -> Option<BundSnippet> {
        extract_bund_snippet(s, DetectOpts::default())
    }
    fn detect_permissive(s: &str) -> Option<BundSnippet> {
        extract_bund_snippet(s, DetectOpts {
            fenced_only: false, slash_strictness: SlashStrictness::Permissive,
        })
    }
    fn detect_fenced_only(s: &str) -> Option<BundSnippet> {
        extract_bund_snippet(s, DetectOpts {
            fenced_only: true, slash_strictness: SlashStrictness::Strict,
        })
    }

    // ── Fenced ─────────────────────────────────────────────────────

    #[test]
    fn fenced_block_extracts_body_and_remainder() {
        let s = "Find cpu.user trends:\n\n```bund\n\"cpu.user\" \"1h\" cls.trends\n```\n\nWhat changed?";
        let snip = detect(s).expect("detected");
        assert_eq!(snip.source, SnippetSource::Fenced);
        assert_eq!(snip.code, "\"cpu.user\" \"1h\" cls.trends");
        assert_eq!(snip.remainder, "Find cpu.user trends:\nWhat changed?");
    }

    #[test]
    fn fenced_block_at_start_of_message_no_preamble() {
        let s = "```bund\n2 40 + .\n```\n";
        let snip = detect(s).unwrap();
        assert_eq!(snip.source, SnippetSource::Fenced);
        assert_eq!(snip.code, "2 40 + .");
        assert_eq!(snip.remainder, "");
    }

    #[test]
    fn fenced_block_language_tag_is_case_insensitive() {
        let s = "```BUND\n40 2 + .\n```";
        let snip = detect(s).unwrap();
        assert_eq!(snip.code, "40 2 + .");
    }

    #[test]
    fn fenced_block_inside_text_picks_up_correctly() {
        let s = "I want to run\n```bund\ncls.knn\n```\non the last hour.";
        let snip = detect(s).unwrap();
        assert_eq!(snip.code, "cls.knn");
        assert_eq!(snip.remainder, "I want to run\non the last hour.");
    }

    #[test]
    fn fenced_with_non_bund_language_is_not_a_snippet() {
        // Generic ``` blocks (no language, or other languages) are
        // operator output — they should NOT trigger eval.
        assert!(detect("```python\nprint(42)\n```").is_none());
        assert!(detect("```\nplain text\n```").is_none());
        assert!(detect("```rust\nfn main() {}\n```").is_none());
    }

    #[test]
    fn fenced_empty_body_is_not_a_snippet() {
        assert!(detect("```bund\n```").is_none());
        assert!(detect("```bund\n   \n   \n```").is_none());
    }

    #[test]
    fn fenced_unterminated_returns_none() {
        assert!(detect("```bund\ncls.knn").is_none());
    }

    #[test]
    fn fenced_inline_triple_backtick_not_at_line_start_ignored() {
        // ```bund must be at the start of a line, not embedded mid-line.
        let s = "see `\\`\\`\\`bund\\`\\`\\``";
        assert!(detect(s).is_none());
    }

    // ── Leading slash, strict ──────────────────────────────────────

    #[test]
    fn slash_then_letter_extracts() {
        let snip = detect("/cls.knn").unwrap();
        assert_eq!(snip.source, SnippetSource::LeadingSlash);
        assert_eq!(snip.code, "cls.knn");
        assert_eq!(snip.remainder, "");
    }

    #[test]
    fn slash_then_dollar_extracts() {
        let snip = detect("/$wb 0 $get").unwrap();
        assert_eq!(snip.code, "$wb 0 $get");
    }

    #[test]
    fn slash_then_quote_extracts() {
        let snip = detect("/\"cpu.user\" \"1h\" cls.trends").unwrap();
        assert_eq!(snip.code, "\"cpu.user\" \"1h\" cls.trends");
    }

    #[test]
    fn slash_then_paren_extracts() {
        let snip = detect("/( 2 40 + . )").unwrap();
        assert_eq!(snip.code, "( 2 40 + . )");
    }

    #[test]
    fn slash_strict_rejects_path_like_input() {
        assert!(detect("/etc/hosts is misconfigured").is_none());
        assert!(detect("/api/v1/items/42 returns 500").is_none());
        assert!(detect("/usr/local/bin/bdsnode died").is_none());
        // The slash followed by `/` (double-slash) — universally bad signal.
        assert!(detect("//comment line").is_none());
    }

    #[test]
    fn slash_strict_rejects_bare_slash() {
        assert!(detect("/").is_none());
        assert!(detect("/ ").is_none());
        assert!(detect("/\n").is_none());
    }

    #[test]
    fn slash_permissive_accepts_path_like_input() {
        // Permissive operators trust their users; we deliver the body
        // verbatim and let the Bund parser error if it's invalid.
        let snip = detect_permissive("/etc/hosts is misconfigured").unwrap();
        assert_eq!(snip.code, "etc/hosts is misconfigured");
    }

    #[test]
    fn slash_with_question_after_blank_line() {
        let s = "/cls.knn \"1h\"\n\nWhat patterns do you see?";
        let snip = detect(s).unwrap();
        assert_eq!(snip.code, "cls.knn \"1h\"");
        assert_eq!(snip.remainder, "What patterns do you see?");
    }

    #[test]
    fn slash_multi_line_body() {
        let s = "/$wb 0 $get\n/2 40 +\n/.\n\nWhy does this not equal 42?";
        let snip = detect(s).unwrap();
        assert_eq!(snip.code, "$wb 0 $get\n2 40 +\n.");
        assert_eq!(snip.remainder, "Why does this not equal 42?");
    }

    #[test]
    fn slash_with_leading_whitespace_still_matches() {
        // Some chat UIs (htmx) might prepend trim-on-paste whitespace.
        let snip = detect("   /cls.knn").unwrap();
        assert_eq!(snip.code, "cls.knn");
    }

    // ── Priority + composition ────────────────────────────────────

    #[test]
    fn fenced_wins_over_slash_when_both_present() {
        let s = "/cls.fingerprints.recent\n\n```bund\ncls.knn\n```\n";
        let snip = detect(s).unwrap();
        // Fenced wins.  The leading `/` line is part of the remainder.
        assert_eq!(snip.source, SnippetSource::Fenced);
        assert_eq!(snip.code, "cls.knn");
        assert!(snip.remainder.contains("/cls.fingerprints.recent"));
    }

    #[test]
    fn fenced_only_mode_ignores_slash() {
        assert!(detect_fenced_only("/cls.knn").is_none());
        let snip = detect_fenced_only("```bund\ncls.knn\n```").unwrap();
        assert_eq!(snip.code, "cls.knn");
    }

    // ── Negative ───────────────────────────────────────────────────

    #[test]
    fn plain_message_is_not_a_snippet() {
        assert!(detect("What's happening with cpu.user in the last hour?").is_none());
        assert!(detect("").is_none());
        assert!(detect("   ").is_none());
    }

    #[test]
    fn slash_alone_in_middle_of_message_does_not_match() {
        // The detector is strict about the leading slash being on the
        // FIRST non-whitespace position.  A `/` mid-message doesn't
        // trigger.
        assert!(detect("Run my script: cls.knn").is_none());
        assert!(detect("Question first /this/is/a/path").is_none());
    }
}
