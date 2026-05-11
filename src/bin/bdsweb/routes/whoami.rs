//! `GET /whoami` — return the currently-authenticated user as JSON.
//!
//! Resolves the user_id stashed in request extensions by
//! `auth::require_session` to a username + display_name by reading
//! `v3/user.list` (HMAC-signed) and matching on id.  The list cache
//! is implicit — we hit the RPC per request but the cluster
//! `users.duckdb` is small.  A simpler v1.
//!
//! When in open-access mode (no shared_secret), or during the
//! first-user bootstrap window, returns `{authenticated: false}` so
//! the nav can omit the username + sign-out button gracefully.

use axum::{extract::State, response::Json};
use axum_extra::extract::CookieJar;
use bdslib::cluster::session::verify_session_token;
use serde_json::{json, Value};

use crate::{admin::signed_rpc, auth::SESSION_COOKIE, state::AppState};

pub async fn whoami(
    State(state): State<AppState>,
    jar:           CookieJar,
) -> Json<Value> {
    // No secret configured (open-access) → nothing to authenticate.
    if state.shared_secret.is_empty() {
        return Json(json!({
            "authenticated": false,
            "mode":          "open-access",
        }));
    }

    // /whoami sits in the auth-middleware allow-list (it must work
    // pre-login so the nav JS can decide whether to render Sign out
    // / Username at all).  That means SessionClaims are NOT in
    // request extensions here; we verify the cookie ourselves.
    let token = match jar.get(SESSION_COOKIE).map(|c| c.value().to_owned()) {
        Some(t) => t,
        None    => return Json(json!({ "authenticated": false })),
    };
    let claims = match verify_session_token(&token, &state.shared_secret) {
        Ok(c)  => c,
        Err(_) => return Json(json!({ "authenticated": false })),
    };

    // Resolve user_id → username + display_name via v3/user.list.
    // Failure here is non-fatal: we still confirm authentication
    // succeeded, we just can't show the nice name.
    let listing = signed_rpc(&state, "v3/user.list", json!({})).await.ok();
    let user_row = listing.as_ref()
        .and_then(|v| v.get("users").and_then(|u| u.as_array()))
        .and_then(|arr| arr.iter().find(|u| {
            u.get("id").and_then(|x| x.as_str()) == Some(claims.user_id.to_string().as_str())
        }))
        .cloned()
        .unwrap_or(Value::Null);

    Json(json!({
        "authenticated": true,
        "user_id":       claims.user_id.to_string(),
        "expires_at":    claims.expires_at,
        "username":      user_row.get("username").cloned().unwrap_or(Value::Null),
        "display_name":  user_row.get("metadata")
            .and_then(|m| m.get("display_name"))
            .cloned()
            .unwrap_or(Value::Null),
    }))
}
