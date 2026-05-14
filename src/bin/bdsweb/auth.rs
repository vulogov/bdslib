//! Session-cookie auth middleware for bdsweb.
//!
//! Every request — except the small allow-list (`/login`, `/logout`,
//! `/version`, static assets) — passes through [`require_session`].
//! When `state.shared_secret` is empty, auth is disabled entirely
//! (open-access mode, used when bdsweb is started without
//! `--config`).  When the secret IS configured, the middleware:
//!
//! 1. Reads the `bds_session` cookie.
//! 2. Verifies its HMAC + expiry via
//!    `bdslib::cluster::session::verify_session_token`.
//! 3. On success: stores the [`bdslib::cluster::session::SessionClaims`]
//!    in request extensions so downstream handlers can read
//!    `req.extensions().get::<SessionClaims>()`.
//! 4. On miss / tampered / expired: redirects to `/login`.
//!
//! First-user bootstrap: while the cluster's user store is empty,
//! the middleware lets every request through unconditionally so an
//! operator can hit `/admin/users` to create the first user.  Once
//! at least one user exists, the bootstrap window closes.  The empty-
//! state probe is cached for 30s to avoid hitting `v3/user.list` on
//! every request.

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::CookieJar;
use bdslib::cluster::session::{verify_session_token, SessionClaims};
use serde_json::json;
use std::net::SocketAddr;
use std::time::Instant;

use crate::state::AppState;

/// Name of the cookie holding the session token.
pub const SESSION_COOKIE: &str = "bds_session";

/// Cache of whether the user store is empty cluster-wide.
///
/// Only the **closed** result (`is_empty = false`) is ever cached, and
/// only for 30 s.  The *open* state — where auth is fully bypassed —
/// is deliberately never cached: while the bootstrap window is open we
/// re-probe `v3/user.list` on every request so that creating the first
/// user closes the window on the very next request, not up to 30 s
/// later (security finding M1).
#[derive(Clone, Copy, Debug)]
pub struct BootstrapCache {
    pub is_empty:   bool,
    pub fetched_at: Instant,
}

impl Default for BootstrapCache {
    fn default() -> Self {
        // Long-past instant forces the first read to refresh.
        Self { is_empty: true, fetched_at: Instant::now() - std::time::Duration::from_secs(3600) }
    }
}

/// Check whether the user store is empty cluster-wide.  Returns `true`
/// when the bootstrap window is still open.
///
/// The closed result is cached for 30 s; the open result is not cached
/// at all — see [`BootstrapCache`].  On any error reaching the node we
/// conservatively return `false` (no bootstrap bypass) so a transient
/// outage doesn't unintentionally open the auth wall.
async fn bootstrap_open(state: &AppState) -> bool {
    {
        let cache = state.bootstrap_cache.read().await;
        // A fresh *closed* verdict short-circuits.  A cached "open"
        // value is ignored — we always re-probe while open.
        if !cache.is_empty && cache.fetched_at.elapsed() < std::time::Duration::from_secs(30) {
            return false;
        }
    }
    // Refresh: HMAC-sign a v3/user.list call.  When the store is
    // empty we get { users: [] }; the bootstrap bypass for
    // v3/user.add then kicks in.  Auth probe failure → assume closed.
    let signed = match crate::admin::signed_rpc(state, "v3/user.list", json!({})).await {
        Ok(v) => v,
        Err(_) => {
            let mut cache = state.bootstrap_cache.write().await;
            cache.fetched_at = Instant::now();
            cache.is_empty   = false;
            return false;
        }
    };
    let is_empty = signed.get("users")
        .and_then(|v| v.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(false);

    // Cache only the closed verdict; leave the open state uncached.
    if !is_empty {
        let mut cache = state.bootstrap_cache.write().await;
        cache.fetched_at = Instant::now();
        cache.is_empty   = false;
    }
    is_empty
}

/// Axum middleware that gates every request behind a valid
/// `bds_session` cookie.  Configured paths bypass the gate so
/// `/login`, `/logout`, and `/version` stay reachable without a
/// session.
pub async fn require_session(
    State(state):       State<AppState>,
    ConnectInfo(peer):  ConnectInfo<SocketAddr>,
    jar:                CookieJar,
    mut req:            Request<Body>,
    next:               Next,
) -> Response {
    // Open-access mode: no shared secret → no auth.
    if state.shared_secret.is_empty() {
        return next.run(req).await;
    }

    // Always-public paths.  /whoami is included because base.html's
    // JS hits it on every page load to decide whether to render the
    // username + Sign out button; it must work BEFORE the visitor
    // has a session cookie (returns `{authenticated: false}` then).
    let path = req.uri().path();
    if matches!(path, "/login" | "/logout" | "/version" | "/whoami" | "/healthz") {
        return next.run(req).await;
    }

    // First-user bootstrap: while no users exist, requests go through
    // so an operator can hit /admin/users to create the first user.
    // Gated to loopback peers (security finding M1) — the auth-bypass
    // window must never be reachable from the network, even briefly.
    // Once the first user lands, the very next request re-probes and
    // this branch closes (the open state is not cached).
    if peer.ip().is_loopback() && bootstrap_open(&state).await {
        return next.run(req).await;
    }

    // Cookie check.
    let token = jar.get(SESSION_COOKIE).map(|c| c.value().to_owned());
    let token = match token {
        Some(t) => t,
        None    => return redirect_to_login(path),
    };
    match verify_session_token(&token, &state.shared_secret) {
        Ok(claims) => {
            req.extensions_mut().insert::<SessionClaims>(claims);
            next.run(req).await
        }
        Err(e) => {
            log::debug!("[auth] session rejected for {path}: {e}");
            redirect_to_login(path)
        }
    }
}

fn redirect_to_login(from_path: &str) -> Response {
    // Preserve the URL the user was trying to reach so /login can
    // redirect back after a successful sign-in.  Skip the `next`
    // hint when bouncing from /login → /login (no loop bait) or
    // from the bare root.
    if from_path == "/" || from_path == "/login" {
        Redirect::to("/login").into_response()
    } else {
        let encoded = urlencoding::encode(from_path);
        Redirect::to(&format!("/login?next={encoded}")).into_response()
    }
}

/// 401 response for HTMX endpoints (under /*/results or POST forms)
/// where a redirect would lose the partial-update context.  Forces
/// the client to follow up with a full reload to /login.  Reserved
/// for a future iteration; currently unused.
#[allow(dead_code)]
pub fn unauthorised() -> Response {
    (StatusCode::UNAUTHORIZED, "authentication required").into_response()
}
