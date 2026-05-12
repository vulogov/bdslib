//! Server-side Markdown → HTML rendering for LLM-produced output.
//!
//! LLMs almost always return GitHub-flavoured markdown (headings,
//! bullet lists, fenced code, **bold**, etc.).  Rendering the raw
//! text in a `<pre>`-style bubble leaves the markup visible and
//! makes the response feel unfinished, so every pane that displays
//! assistant output runs the text through [`render`] before
//! interpolating into a template (via askama's `|safe` filter).
//!
//! ## Safety
//!
//! pulldown-cmark with `ENABLE_HTML` off does NOT parse HTML, but it
//! also does NOT escape it — raw `<script>` in the source markdown
//! lands verbatim in the output.  Because LLM responses are
//! effectively untrusted text (Ollama, DeepSeek, etc. can return
//! whatever they hallucinate, and the operator pastes search results
//! that originated outside the system), we then run pulldown's
//! output through `ammonia` with its default allowlist.  Ammonia is
//! the de-facto safe HTML sanitiser in Rust — it strips
//! `<script>`, JavaScript URLs, `on*` event handlers, and similar
//! markup before the HTML reaches the template.
//!
//! GFM tables + strikethrough + task lists are enabled because
//! they're common in SRE / analysis output ("|Service|Errors|" rows,
//! "~~deprecated~~", "- [x] checked").  Footnotes intentionally
//! disabled — LLMs almost never emit them and they confuse the
//! parser when bracketed text incidentally matches the footnote
//! syntax.

use pulldown_cmark::{html, Options, Parser};

/// Convert a markdown string to a sanitised HTML fragment ready to
/// interpolate into a template with askama's `|safe` filter.
pub fn render(md: &str) -> String {
    if md.is_empty() {
        return String::new();
    }
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    // ENABLE_HTML deliberately omitted — ammonia runs after, but
    // letting pulldown re-emit HTML elements would just give ammonia
    // more to strip.

    let parser = Parser::new_ext(md, opts);
    let mut raw = String::with_capacity(md.len() + md.len() / 4);
    html::push_html(&mut raw, parser);

    // Sanitise: strip <script>, on* handlers, javascript: URLs, etc.
    // Ammonia's default allowlist is GitHub-style: lets through the
    // tags we actually want (p, h1-h6, ul/ol/li, code, pre,
    // blockquote, table, th, td, strong, em, a[href], img[src], …).
    ammonia::clean(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_bold_and_lists() {
        let h = render("**bold** and\n- one\n- two");
        assert!(h.contains("<strong>bold</strong>"));
        assert!(h.contains("<li>one</li>"));
        assert!(h.contains("<li>two</li>"));
    }

    #[test]
    fn renders_fenced_code() {
        let h = render("```\necho hi\n```");
        assert!(h.contains("<pre><code>echo hi"));
    }

    #[test]
    fn renders_table() {
        let h = render("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(h.contains("<table>"));
        assert!(h.contains("<th>a</th>"));
        assert!(h.contains("<td>1</td>"));
    }

    #[test]
    fn raw_script_tag_is_stripped() {
        // Ammonia drops <script> entirely; pulldown's raw passthrough
        // means we'd otherwise have live markup in the output.
        let h = render("hello <script>alert(1)</script> world");
        assert!(!h.contains("<script>"), "rendered: {h}");
        assert!(!h.contains("alert(1)"), "rendered: {h}");
        assert!(h.contains("hello"));
        assert!(h.contains("world"));
    }

    #[test]
    fn javascript_url_in_link_is_stripped() {
        // GFM auto-link / explicit link with a javascript: scheme
        // gets neutralised — the link tag stays but the href is
        // removed by ammonia's URL filter.
        let h = render("[click](javascript:alert(1))");
        assert!(!h.contains("javascript:"), "rendered: {h}");
    }

    #[test]
    fn inline_event_handler_is_stripped() {
        let h = render("<img src=x onerror=alert(1)>");
        assert!(!h.contains("onerror"), "rendered: {h}");
    }

    #[test]
    fn empty_input_returns_empty_string() {
        assert_eq!(render(""), "");
    }
}
