//! Parse Bund eval errors into typed segments so the `/bund` page can
//! render them with semantic colors.
//!
//! Bund errors come back from `v2/eval` as a single opaque string —
//! readable but visually flat.  Two patterns dominate:
//!
//! 1. Token-evaluation failures from `bund_compile_and_eval`:
//!    `Attempt to evaluate value Value { … data: String("set") … }
//!     returned error: SET key expected to be string: This Dynamic
//!     type is not string (/path/to/value_dict.rs:24:45)
//!     (src/vm/helpers/eval.rs:106:33)`
//!
//! 2. Inline-word lookup failures:
//!    `i(.stack&) for stack returned: Inline .stack& not registered
//!     (/…/ts_inline.rs:39:9)`
//!
//! Plus a long tail of plain `bail!()` strings.  We try the two
//! structured patterns first, extract the **token** (the word that
//! failed), the **reason**, and any trailing **source locations**, and
//! fall back to a single `Text` segment so the user still sees the
//! raw error.
//!
//! The output is a `Vec<ErrorSegment>` consumed by the askama template
//! in `partials/bund_result.html`.  Each kind maps to a CSS class.

#[derive(Debug, Clone)]
pub struct ErrorSegment {
    /// Wire class — matches the `class="bund-err-<kind>"` rule
    /// in the partial template's <style> block.
    pub kind: &'static str,
    pub text: String,
}

impl ErrorSegment {
    fn text<S: Into<String>>(s: S) -> Self { Self { kind: "text",   text: s.into() } }
    fn token<S: Into<String>>(s: S) -> Self { Self { kind: "token",  text: s.into() } }
    fn reason<S: Into<String>>(s: S) -> Self { Self { kind: "reason", text: s.into() } }
    fn source<S: Into<String>>(s: S) -> Self { Self { kind: "source", text: s.into() } }
}

/// Top-level parser.  Always returns at least one segment so the
/// template never renders an empty error block.
pub fn parse_bund_error(raw: &str) -> Vec<ErrorSegment> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return vec![ErrorSegment::text("(empty error)")];
    }

    if let Some(segs) = parse_evaluate_value(trimmed) {
        return segs;
    }
    if let Some(segs) = parse_inline_stack(trimmed) {
        return segs;
    }
    fallback(trimmed)
}

/// `Attempt to evaluate value Value { … data: <Type>("<word>") … } returned error: <reason>`
fn parse_evaluate_value(raw: &str) -> Option<Vec<ErrorSegment>> {
    let head = "Attempt to evaluate value Value {";
    if !raw.starts_with(head) { return None; }
    let after_head = &raw[head.len()..];

    // Pull out `data: …,` — the data field encodes the word that
    // failed.  We accept any of the rust_dynamic Val variants that
    // serialize as `Name(...)` via Debug.
    let token = extract_data_field(after_head);

    // Locate `returned error:` separator.
    let needle = " returned error: ";
    let sep = after_head.find(needle)?;
    let reason_full = &after_head[sep + needle.len() ..];

    // Strip trailing `(file:line:col)` location suffixes — sometimes
    // chained, sometimes none.  Anything that remains is the reason.
    let (reason, sources) = split_trailing_locations(reason_full);

    let mut out = Vec::with_capacity(4 + sources.len() * 2);
    out.push(ErrorSegment::text("while evaluating "));
    if let Some(tok) = token {
        out.push(ErrorSegment::token(tok));
        out.push(ErrorSegment::text(" — "));
    }
    out.push(ErrorSegment::reason(reason.trim()));
    for s in sources {
        out.push(ErrorSegment::text(" at "));
        out.push(ErrorSegment::source(s));
    }
    Some(out)
}

/// `i(<token>) for stack returned: <reason>` (or variants from
/// rust_multistackvm).
fn parse_inline_stack(raw: &str) -> Option<Vec<ErrorSegment>> {
    if !raw.starts_with("i(") { return None; }
    let close = raw.find(") for stack returned: ")?;
    let token = &raw[2..close];
    let reason_full = &raw[close + ") for stack returned: ".len() ..];
    let (reason, sources) = split_trailing_locations(reason_full);

    let mut out = Vec::with_capacity(4 + sources.len() * 2);
    out.push(ErrorSegment::text("inline word "));
    out.push(ErrorSegment::token(token));
    out.push(ErrorSegment::text(" — "));
    out.push(ErrorSegment::reason(reason.trim()));
    for s in sources {
        out.push(ErrorSegment::text(" at "));
        out.push(ErrorSegment::source(s));
    }
    Some(out)
}

/// Plain string error: still pull out any `(file:line:col)`
/// suffixes so they render in the source style.
fn fallback(raw: &str) -> Vec<ErrorSegment> {
    let (body, sources) = split_trailing_locations(raw);
    let mut out = Vec::with_capacity(1 + sources.len() * 2);
    out.push(ErrorSegment::reason(body.trim()));
    for s in sources {
        out.push(ErrorSegment::text(" at "));
        out.push(ErrorSegment::source(s));
    }
    out
}

/// Find `data: <SomeName>("..."),` or `data: <SomeName>(...)` and
/// return the inside.  Handles String, Token, I64, F64, etc.
///
/// Match shape: `data: <Variant>(<inner>)` where `<inner>` can be a
/// quoted string or a bare literal.  We don't need 100% accuracy —
/// when the structure isn't quite what we expect we just return None
/// and the caller falls back to showing the raw reason.
fn extract_data_field(s: &str) -> Option<String> {
    let after = s.find("data: ").map(|i| &s[i + "data: ".len()..])?;
    // Read variant name up to `(`
    let paren = after.find('(')?;
    let inner_start = paren + 1;
    let inner = &after[inner_start..];

    // Balanced paren scan so payloads containing `(` / `)` (rare for
    // strings, plausible for nested types) come through cleanly.
    let mut depth: i32 = 1;
    let mut end: Option<usize> = None;
    for (i, ch) in inner.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 { end = Some(i); break; }
            }
            _ => {}
        }
    }
    let end = end?;
    let body = &inner[..end];
    // Strip a single pair of surrounding double quotes when present
    // (the String / Token variants are emitted as `String("foo")`).
    let unquoted = body.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
        .unwrap_or(body);
    if unquoted.is_empty() { None } else { Some(unquoted.to_owned()) }
}

/// Trim any number of trailing `(...:digit:digit)` Rust-style source
/// locations off `s` and return `(remaining, locations_in_order)`.
/// Tolerant of separators between them (whitespace, single spaces).
fn split_trailing_locations(s: &str) -> (String, Vec<String>) {
    let mut sources: Vec<String> = Vec::new();
    let mut cur = s.trim_end().to_owned();
    loop {
        if !cur.ends_with(')') { break; }
        // Find the matching `(` for the trailing `)`.
        let open = match find_matching_open(&cur) {
            Some(i) => i,
            None    => break,
        };
        let candidate = &cur[open + 1..cur.len() - 1];
        if !looks_like_file_line_col(candidate) { break; }
        sources.push(candidate.to_owned());
        cur = cur[..open].trim_end().to_owned();
    }
    sources.reverse();
    (cur, sources)
}

/// Treat `<path>:<int>:<int>` as a source location.  Tolerant of
/// Windows-style paths with `:` in them by requiring the last two
/// colon-separated trailing segments to both parse as integers.
fn looks_like_file_line_col(s: &str) -> bool {
    let mut parts = s.rsplitn(3, ':');
    let col  = parts.next().unwrap_or("");
    let line = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");
    !rest.is_empty()
        && col.chars().all(|c| c.is_ascii_digit())  && !col.is_empty()
        && line.chars().all(|c| c.is_ascii_digit()) && !line.is_empty()
}

/// Find the index of the `(` that matches the LAST character of `s`
/// (which the caller has already checked is `)`).  Scans backwards
/// counting paren depth; returns None when the parens are unbalanced.
fn find_matching_open(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes[bytes.len() - 1] != b')' { return None; }
    let mut depth: i32 = 0;
    // Walk backwards from the closing paren.
    for (i, &b) in bytes.iter().enumerate().rev() {
        match b {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 { return Some(i); }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_attempt_to_evaluate_with_one_location() {
        let raw = r#"Attempt to evaluate value Value { id: "x", stamp: 1.0, dt: 6, q: 100.0, data: String("set"), attr: [], curr: -1, tags: {} } returned error: SET key expected to be string (/path/value_dict.rs:24:45)"#;
        let segs = parse_bund_error(raw);
        // Expect: text "while evaluating ", token "set", text " — ", reason "...", text " at ", source "..."
        assert!(segs.iter().any(|s| s.kind == "token" && s.text == "set"),
            "missing token segment: {segs:?}");
        assert!(segs.iter().any(|s| s.kind == "reason" && s.text.contains("SET key expected to be string")),
            "missing reason: {segs:?}");
        assert!(segs.iter().any(|s| s.kind == "source" && s.text == "/path/value_dict.rs:24:45"),
            "missing source: {segs:?}");
    }

    #[test]
    fn parses_attempt_to_evaluate_with_multiple_locations() {
        let raw = r#"Attempt to evaluate value Value { id: "y", stamp: 2.0, dt: 6, q: 100.0, data: String("convert.to_dict"), attr: [], curr: -1, tags: {} } returned error: CONVERT.TO_DICT returned error: Can not convert string to 26 (/Users/foo/convert/internal.rs:33:21) (src/vm/helpers/eval.rs:106:33)"#;
        let segs = parse_bund_error(raw);
        let token = segs.iter().find(|s| s.kind == "token").unwrap();
        assert_eq!(token.text, "convert.to_dict");
        let sources: Vec<&str> = segs.iter().filter(|s| s.kind == "source").map(|s| s.text.as_str()).collect();
        assert_eq!(sources, vec![
            "/Users/foo/convert/internal.rs:33:21",
            "src/vm/helpers/eval.rs:106:33",
        ]);
    }

    #[test]
    fn parses_inline_stack_error() {
        let raw = "i(.stack&) for stack returned: Inline .stack& not registered (/cargo/.../ts_inline.rs:39:9)";
        let segs = parse_bund_error(raw);
        assert!(segs.iter().any(|s| s.kind == "token" && s.text == ".stack&"));
        assert!(segs.iter().any(|s| s.kind == "reason" && s.text.contains("Inline .stack& not registered")));
        assert!(segs.iter().any(|s| s.kind == "source"));
    }

    #[test]
    fn falls_back_to_reason_only_for_plain_string() {
        let raw = "something unparseable happened";
        let segs = parse_bund_error(raw);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].kind, "reason");
        assert_eq!(segs[0].text, "something unparseable happened");
    }

    #[test]
    fn empty_input_renders_placeholder_not_panic() {
        let segs = parse_bund_error("");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "(empty error)");
    }

    #[test]
    fn location_extractor_rejects_non_location_parens() {
        // (something) at the end that isn't file:line:col MUST stay
        // in the reason — otherwise we'd lose context.
        let (body, sources) = split_trailing_locations("something (and a note)");
        assert!(sources.is_empty());
        assert_eq!(body, "something (and a note)");
    }
}
