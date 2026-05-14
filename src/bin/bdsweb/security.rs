//! Cross-cutting HTTP security for bdsweb — middleware + helpers.
//!
//! Two middleware layers, both applied to the fully-composed router
//! (so they also cover `/login`, `/logout`, and the rate-limited
//! sub-router):
//!
//! * [`require_same_origin`] — CSRF backstop.  Every state-changing
//!   request (POST/PUT/PATCH/DELETE) must carry an `Origin` (or, as a
//!   fallback, `Referer`) whose host matches the request's `Host`.
//!   A cross-site `<form>` auto-submit or `fetch()` always carries the
//!   attacker's `Origin`, so this rejects it.  Combined with the fact
//!   that bdsweb has no side-effecting GET routes (the `/*/analyze`
//!   endpoints and `/chat/reset` are POST), this closes the CSRF
//!   surface without a per-form token.
//!
//! * [`set_security_headers`] — adds `X-Frame-Options`,
//!   `X-Content-Type-Options`, `Referrer-Policy`, and a minimal CSP
//!   (`frame-ancestors 'none'`) to every response.
//!
//! Plus [`json_for_script`] — serialises a value to JSON safe for
//! interpolation inside an inline `<script>` block.

use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Extract the bare host (no scheme, no port, no path/userinfo) from
/// an `Origin` value (`scheme://host[:port]`) or a `Referer` value
/// (a full URL).  Returns `None` for anything not `http(s)://…`.
fn host_of(value: &str) -> Option<&str> {
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    // Drop any `user:pass@` userinfo prefix, then the `:port` suffix.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port.split(':').next().unwrap_or(host_port);
    if host.is_empty() { None } else { Some(host) }
}

/// True when `source` (an `Origin` or `Referer` value) is same-host as
/// the `Host` header.  Host comparison is case-insensitive and ignores
/// the port so a TLS terminator (`Host: app.example`, `Origin:
/// https://app.example`) still matches.
fn same_host(host_header: &str, source: &str) -> bool {
    let want = host_header.split(':').next().unwrap_or(host_header);
    match host_of(source) {
        Some(got) => got.eq_ignore_ascii_case(want),
        None => false,
    }
}

/// CSRF backstop — see module docs.  Safe methods pass through
/// untouched; state-changing methods are rejected with `403` unless
/// their `Origin`/`Referer` host matches `Host`.
pub async fn require_same_origin(req: Request<Body>, next: Next) -> Response {
    if matches!(
        *req.method(),
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    ) {
        return next.run(req).await;
    }

    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let source = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get(header::REFERER).and_then(|v| v.to_str().ok()));

    let ok = matches!((host, source), (Some(h), Some(s)) if same_host(h, s));
    if ok {
        return next.run(req).await;
    }

    log::warn!(
        "[csrf] rejected {} {} — Origin/Referer not same-origin (host={:?}, origin/referer={:?})",
        req.method(),
        req.uri().path(),
        host,
        source,
    );
    (StatusCode::FORBIDDEN, "cross-origin request rejected").into_response()
}

/// Adds defence-in-depth response headers to every reply.
///
/// The CSP is intentionally minimal — just `frame-ancestors 'none'`
/// (clickjacking protection, redundant with `X-Frame-Options` but
/// honoured by newer browsers).  A full resource-restricting CSP is a
/// follow-up: bdsweb currently loads Tailwind + htmx from CDNs and uses
/// inline `<script>`/`<style>`, so a `default-src 'self'` policy would
/// need those assets vendored first.
pub async fn set_security_headers(req: Request<Body>, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert("X-Frame-Options", HeaderValue::from_static("DENY"));
    h.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    h.insert("Referrer-Policy", HeaderValue::from_static("same-origin"));
    h.insert(
        "Content-Security-Policy",
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    resp
}

/// Serialise `value` to JSON for safe interpolation inside an inline
/// `<script>` block (templates that do `{{ x_json|safe }}`).
///
/// `serde_json` does not escape `<`, `>`, or `&`, so a string field
/// holding `</script>` would break out of the script context — latent
/// stored XSS the moment any such field carries attacker-influenced
/// text.  We post-escape those three plus the JS line/paragraph
/// separators U+2028/U+2029 (valid in JSON strings but illegal raw in
/// a JS string literal).  Escaping `<` alone already neutralises
/// `</script>`; the rest is defence-in-depth.
pub fn json_for_script<T: serde::Serialize>(value: &T) -> serde_json::Result<String> {
    let raw = serde_json::to_string(value)?;
    Ok(raw
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_for_script_escapes_script_breakout() {
        let payload = serde_json::json!({ "k": "</script><img src=x onerror=alert(1)>" });
        let out = json_for_script(&payload).unwrap();
        assert!(!out.contains("</script>"));
        assert!(!out.contains('<'));
        assert!(!out.contains('>'));
        assert!(out.contains("\\u003c/script\\u003e"));
        // Still valid JSON that round-trips to the original value.
        let back: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn json_for_script_escapes_line_separators() {
        let out = json_for_script(&"a\u{2028}b\u{2029}c").unwrap();
        assert!(!out.contains('\u{2028}'));
        assert!(!out.contains('\u{2029}'));
        assert_eq!(serde_json::from_str::<String>(&out).unwrap(), "a\u{2028}b\u{2029}c");
    }

    #[test]
    fn json_for_script_passes_plain_numbers() {
        assert_eq!(json_for_script(&[1, 2, 3]).unwrap(), "[1,2,3]");
    }

    #[test]
    fn host_of_parses_origin_and_referer() {
        assert_eq!(host_of("https://app.example"), Some("app.example"));
        assert_eq!(host_of("http://127.0.0.1:8080"), Some("127.0.0.1"));
        assert_eq!(host_of("https://app.example/login?next=/x"), Some("app.example"));
        assert_eq!(host_of("https://user:pw@app.example:9/x"), Some("app.example"));
        assert_eq!(host_of("ftp://app.example"), None);
        assert_eq!(host_of("not-a-url"), None);
        assert_eq!(host_of("https://"), None);
    }

    #[test]
    fn same_host_matches_ignoring_port_and_case() {
        assert!(same_host("app.example", "https://app.example"));
        assert!(same_host("app.example:443", "https://APP.example"));
        assert!(same_host("127.0.0.1:8080", "http://127.0.0.1:8080"));
        assert!(!same_host("app.example", "https://evil.example"));
        assert!(!same_host("app.example", "https://app.example.evil.com"));
        assert!(!same_host("app.example", "//app.example"));
        assert!(!same_host("app.example", "garbage"));
    }
}
