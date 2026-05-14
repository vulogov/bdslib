use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Response},
};

#[derive(Debug)]
#[allow(dead_code)]
pub enum AppError {
    Rpc(String),
    Http(reqwest::Error),
    Json(serde_json::Error),
    Render(askama::Error),
    Msg(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Rpc(m)    => write!(f, "RPC error: {m}"),
            AppError::Http(e)   => write!(f, "HTTP error: {e}"),
            AppError::Json(e)   => write!(f, "JSON parse error: {e}"),
            AppError::Render(e) => write!(f, "Template error: {e}"),
            AppError::Msg(m)    => write!(f, "{m}"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Log the full detail server-side; show the browser only a
        // generic, class-appropriate message.  The raw error can name
        // the backend node URL or echo internal response text —
        // neither belongs in an error page a visitor sees (M2).
        log::warn!("request failed: {self}");
        let client_msg = match &self {
            AppError::Http(_)   => "Couldn't reach the bdsnode backend. \
                                    It may be restarting — try again in a moment.",
            AppError::Rpc(_)    => "The bdsnode backend returned an error.",
            AppError::Json(_)   => "The bdsnode backend returned an unexpected response.",
            AppError::Render(_) => "The page failed to render.",
            AppError::Msg(_)    => "Something went wrong handling this request.",
        };
        let body = format!(
            r#"<!doctype html><html lang="en">
<head><meta charset="UTF-8"><title>Error — bdsnode</title>
<script src="https://cdn.tailwindcss.com"></script></head>
<body class="bg-gray-950 text-red-400 p-10 font-mono">
<h1 class="text-2xl mb-4 text-red-300">Internal Error</h1>
<pre class="text-sm whitespace-pre-wrap">{client_msg}</pre>
<a href="/" class="mt-6 inline-block text-blue-400 underline">← back to dashboard</a>
</body></html>"#
        );
        (StatusCode::INTERNAL_SERVER_ERROR, Html(body)).into_response()
    }
}

impl From<reqwest::Error>    for AppError { fn from(e: reqwest::Error)    -> Self { AppError::Http(e) } }
impl From<serde_json::Error> for AppError { fn from(e: serde_json::Error) -> Self { AppError::Json(e) } }
impl From<askama::Error>     for AppError { fn from(e: askama::Error)     -> Self { AppError::Render(e) } }

/// Middleware: when an HTMX-initiated request (`HX-Request: true`)
/// produces a `500`, replace the full-page error document with a
/// compact inline fragment.  Without this, the entire `<!doctype html>`
/// error page gets swapped into whatever `<div>` the HTMX call
/// targeted — a broken, confusing panel that doesn't recover (H3).
///
/// Layered *outside* `CatchPanicLayer` so it reshapes both `AppError`
/// 500s and caught panics.  Non-HTMX requests and non-500 responses
/// pass through untouched.
pub async fn htmx_error_fragment(req: Request<Body>, next: Next) -> Response {
    let is_htmx = req
        .headers()
        .get("HX-Request")
        .and_then(|v| v.to_str().ok())
        == Some("true");
    let resp = next.run(req).await;
    if !is_htmx || resp.status() != StatusCode::INTERNAL_SERVER_ERROR {
        return resp;
    }
    let fragment = r#"<div class="rounded border border-red-500/40 bg-red-500/10 p-4 text-sm text-red-300">
  <p class="font-medium text-red-200">This section couldn't load.</p>
  <p class="mt-1 text-red-300/80">The bdsnode backend may be unavailable or restarting.</p>
  <button type="button" onclick="location.reload()"
          class="mt-3 rounded bg-red-500/20 px-3 py-1 text-xs text-red-200 hover:bg-red-500/30">
    Reload page
  </button>
</div>"#;
    (StatusCode::INTERNAL_SERVER_ERROR, Html(fragment)).into_response()
}
