//! Administration → User management page.
//!
//! Lists users (hash-free via `v3/user.list`), exposes inline forms for
//! adding a user, deleting a user, and resetting a user's password.
//! All mutations are HMAC-signed via `admin::signed_rpc` so the user
//! visiting this page acts under the same trust boundary as
//! `bdscmd user --secret …`.
//!
//! Future iterations should layer RBAC here so only specific session
//! users can hit these mutation endpoints; Phase 7 ships flat
//! authentication, so every logged-in user can manage every user.

use askama::Template;
use axum::{
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;
use serde_json::json;

use crate::{admin::signed_rpc, error::AppError, state::AppState};

#[derive(Template)]
#[template(path = "admin_users.html")]
struct AdminUsersPage {
    users:     Vec<UserRow>,
    error_msg: String,
    has_error: bool,
    notice:    String,
    has_notice: bool,
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub id:           String,
    pub username:     String,
    pub auth_method:  String,
    pub display_name: String,
    pub disabled:     bool,
    pub created_at:   String,
    pub updated_at:   String,
}

#[allow(dead_code)]
pub async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    render_page(&state, "", "").await
}

async fn render_page(state: &AppState, error_msg: &str, notice: &str) -> Result<Html<String>, AppError> {
    let resp = signed_rpc(state, "v3/user.list", json!({})).await;
    let (users, list_err) = match resp {
        Ok(v) => (parse_users(&v), String::new()),
        Err(e) => (vec![], format!("listing users failed: {e}")),
    };
    let combined_err = if !error_msg.is_empty() { error_msg.to_owned() } else { list_err };
    Ok(Html(AdminUsersPage {
        users,
        has_error:  !combined_err.is_empty(),
        error_msg:  combined_err,
        has_notice: !notice.is_empty(),
        notice:     notice.to_owned(),
    }.render()?))
}

fn parse_users(v: &serde_json::Value) -> Vec<UserRow> {
    let arr = match v.get("users").and_then(|x| x.as_array()) {
        Some(a) => a,
        None    => return vec![],
    };
    arr.iter().filter_map(|u| {
        let id = u.get("id")?.as_str()?.to_owned();
        let username = u.get("username")?.as_str()?.to_owned();
        let auth_method = u.get("auth_method").and_then(|x| x.as_str()).unwrap_or("password").to_owned();
        let display_name = u.get("metadata")
            .and_then(|m| m.get("display_name"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_owned();
        let disabled = u.get("disabled").and_then(|x| x.as_bool()).unwrap_or(false);
        let created_at = u.get("created_at").and_then(|x| x.as_u64())
            .map(crate::client::fmt_ts).unwrap_or_default();
        let updated_at = u.get("updated_at").and_then(|x| x.as_u64())
            .map(crate::client::fmt_ts).unwrap_or_default();
        Some(UserRow { id, username, auth_method, display_name, disabled, created_at, updated_at })
    }).collect()
}

// ── Add ───────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AddForm {
    username:     String,
    password:     String,
    #[serde(default)]
    display_name: String,
}

pub async fn add(
    State(state): State<AppState>,
    Form(form):    Form<AddForm>,
) -> Response {
    if form.username.trim().is_empty() || form.password.is_empty() {
        return error_redirect("username and password are required").into_response();
    }
    let mut params = serde_json::Map::new();
    params.insert("username".into(), json!(form.username));
    params.insert("password".into(), json!(form.password));
    params.insert("auth_method".into(), json!("password"));
    if !form.display_name.is_empty() {
        params.insert("metadata".into(), json!({"display_name": form.display_name}));
    }
    match signed_rpc(&state, "v3/user.add", serde_json::Value::Object(params)).await {
        Ok(_)  => Redirect::to("/admin/users?notice=added").into_response(),
        Err(e) => error_redirect(&format!("add failed: {e}")).into_response(),
    }
}

// ── Delete ────────────────────────────────────────────────────────────────────

pub async fn delete(
    State(state): State<AppState>,
    Path(id):      Path<String>,
) -> Response {
    match signed_rpc(&state, "v3/user.delete", json!({"id": id})).await {
        Ok(_)  => Redirect::to("/admin/users?notice=deleted").into_response(),
        Err(e) => error_redirect(&format!("delete failed: {e}")).into_response(),
    }
}

// ── Reset password ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ResetForm {
    password: String,
}

pub async fn reset_password(
    State(state): State<AppState>,
    Path(id):      Path<String>,
    Form(form):    Form<ResetForm>,
) -> Response {
    if form.password.is_empty() {
        return error_redirect("password cannot be empty").into_response();
    }
    match signed_rpc(&state, "v3/user.modify", json!({
        "id": id, "password": form.password,
    })).await {
        Ok(_)  => Redirect::to("/admin/users?notice=password-reset").into_response(),
        Err(e) => error_redirect(&format!("reset failed: {e}")).into_response(),
    }
}

// ── Enable / disable ──────────────────────────────────────────────────────────

pub async fn disable(
    State(state): State<AppState>,
    Path(id):      Path<String>,
) -> Response {
    match signed_rpc(&state, "v3/user.modify", json!({"id": id, "disabled": true})).await {
        Ok(_)  => Redirect::to("/admin/users?notice=disabled").into_response(),
        Err(e) => error_redirect(&format!("disable failed: {e}")).into_response(),
    }
}

pub async fn enable(
    State(state): State<AppState>,
    Path(id):      Path<String>,
) -> Response {
    match signed_rpc(&state, "v3/user.modify", json!({"id": id, "disabled": false})).await {
        Ok(_)  => Redirect::to("/admin/users?notice=enabled").into_response(),
        Err(e) => error_redirect(&format!("enable failed: {e}")).into_response(),
    }
}

fn error_redirect(msg: &str) -> Redirect {
    let encoded = urlencoding::encode(msg);
    Redirect::to(&format!("/admin/users?error={encoded}"))
}

// ── Page with query-param-driven banners ──────────────────────────────────────
//
// /admin/users?notice=added | deleted | password-reset | disabled | enabled
// /admin/users?error=<msg>

#[derive(Deserialize, Default)]
pub struct PageQuery {
    #[serde(default)]
    pub notice: String,
    #[serde(default)]
    pub error:  String,
}

pub async fn page_with_banners(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<PageQuery>,
) -> Result<Html<String>, AppError> {
    let notice = match q.notice.as_str() {
        "added"          => "User added.",
        "deleted"        => "User deleted.",
        "password-reset" => "Password reset.",
        "disabled"       => "User disabled.",
        "enabled"        => "User re-enabled.",
        _ => "",
    };
    render_page(&state, &q.error, notice).await
}
