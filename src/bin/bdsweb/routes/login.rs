//! `/login` (GET + POST) and `/logout` (POST).
//!
//! GET /login renders a small form.  POST /login submits username +
//! password to `v3/user.authenticate`, drops the returned token into a
//! `bds_session` cookie, and redirects to `?next=` if present
//! (otherwise `/`).
//!
//! POST /logout clears the cookie + redirects to `/login`.
//!
//! When the auth middleware sees an empty user store, both routes
//! still work; the middleware just stops bouncing other pages to
//! `/login` and the operator can use `/admin/users` directly to
//! create the first user.

use askama::Template;
use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use axum_extra::extract::{cookie::{Cookie, SameSite}, CookieJar};
use serde::Deserialize;
use serde_json::json;

use crate::{auth::SESSION_COOKIE, client::rpc, error::AppError, state::AppState};

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    error_msg: String,
    has_error: bool,
    next:      String,
    /// True when bdsweb has no `cluster.shared_secret` configured —
    /// surface the open-access banner so operators understand why
    /// they aren't being asked to log in.
    open_access: bool,
}

#[derive(Deserialize, Default)]
pub struct LoginQuery {
    /// Path the user was trying to reach before being bounced here.
    /// We URL-encode + redirect after a successful sign-in.
    #[serde(default)]
    next: String,
}

pub async fn page(
    State(state): State<AppState>,
    Query(q):    Query<LoginQuery>,
) -> Result<Html<String>, AppError> {
    Ok(Html(LoginPage {
        error_msg:   String::new(),
        has_error:   false,
        next:        q.next,
        open_access: state.shared_secret.is_empty(),
    }.render()?))
}

#[derive(Deserialize)]
pub struct LoginForm {
    username:  String,
    password:  String,
    /// Mirrored hidden field from the `next` query param.
    #[serde(default)]
    next:      String,
}

pub async fn submit(
    State(state): State<AppState>,
    jar:           CookieJar,
    Form(form):    Form<LoginForm>,
) -> Response {
    // Open-access mode: nothing to verify; just redirect home.
    if state.shared_secret.is_empty() {
        return (jar, Redirect::to("/")).into_response();
    }

    let resp = match rpc(&state, "v3/user.authenticate", json!({
        "username": form.username,
        "password": form.password,
    })).await {
        Ok(v) => v,
        Err(e) => return render_error(&state, &format!("{e}"), &form.next).await,
    };

    if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return render_error(&state, "Invalid credentials.", &form.next).await;
    }
    let token = match resp.get("session_token").and_then(|v| v.as_str()) {
        Some(t) => t.to_owned(),
        None    => return render_error(&state, "Login succeeded but the server didn't return a session token.", &form.next).await,
    };
    let ttl = resp.get("ttl_secs").and_then(|v| v.as_u64()).unwrap_or(8 * 3600);

    let dest = if form.next.is_empty() || form.next == "/login" { "/".to_owned() } else { form.next };
    let cookie = Cookie::build((SESSION_COOKIE, token))
        .path("/")
        .http_only(true)
        // We CANNOT set Secure unconditionally because bdsweb may be
        // proxied behind a TLS terminator that strips https on the
        // backend leg.  Operators serving over HTTP-only loopback
        // need cookies to work too.  Leave it off here; production
        // deployments should set `Secure` via the proxy (e.g. nginx
        // `proxy_cookie_flags`).
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(ttl as i64))
        .build();
    (jar.add(cookie), Redirect::to(&dest)).into_response()
}

pub async fn logout(jar: CookieJar) -> Response {
    let cookie = Cookie::build((SESSION_COOKIE, ""))
        .path("/")
        .max_age(time::Duration::ZERO)
        .build();
    (jar.add(cookie), Redirect::to("/login")).into_response()
}

async fn render_error(state: &AppState, msg: &str, next: &str) -> Response {
    let body = LoginPage {
        error_msg:   msg.to_owned(),
        has_error:   true,
        next:        next.to_owned(),
        open_access: state.shared_secret.is_empty(),
    }
    .render();
    match body {
        Ok(html) => Html(html).into_response(),
        Err(e)   => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("template error: {e}"))
                        .into_response(),
    }
}

