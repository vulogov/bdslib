use askama::Template;
use axum::{
    extract::{Path, Query, State},
    response::Html,
    Form,
};
use serde::Deserialize;
use serde_json::json;

use crate::{client::{rpc, SESSION}, error::AppError, state::AppState};

// ── Page (full shell) ─────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "scripts.html")]
struct ScriptsPage {}

pub async fn page() -> Result<Html<String>, AppError> {
    Ok(Html(ScriptsPage {}.render()?))
}

// ── List (left column) ────────────────────────────────────────────────────────

struct ScriptListItem {
    id:       String,
    name:     String,
    schedule: String,
}

#[derive(Template)]
#[template(path = "partials/scripts_list.html")]
struct ScriptList {
    scripts: Vec<ScriptListItem>,
    is_empty: bool,
}

pub async fn list(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let resp = rpc(&state, "v2/scripts", json!({ "session": SESSION })).await?;

    let mut items: Vec<ScriptListItem> = Vec::new();
    if let Some(arr) = resp.get("scripts").and_then(|v| v.as_array()) {
        for v in arr {
            items.push(ScriptListItem {
                id:       v.get("id").and_then(|s| s.as_str()).unwrap_or("").to_owned(),
                name:     v.get("name").and_then(|s| s.as_str()).unwrap_or("").to_owned(),
                schedule: v.get("schedule").and_then(|s| s.as_str()).unwrap_or("").to_owned(),
            });
        }
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    let is_empty = items.is_empty();

    Ok(Html(ScriptList { scripts: items, is_empty }.render()?))
}

// ── Editor (right column) ─────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/scripts_editor.html")]
struct ScriptEditor {
    /// Script id ("" when creating a new one).
    id:       String,
    /// Editable fields.
    name:     String,
    schedule: String,
    body:     String,
    /// Banner shown after save / delete.
    flash:    String,
    has_flash: bool,
    is_new:   bool,
}

#[derive(Deserialize)]
pub struct EditorParams {
    #[serde(default)]
    flash: String,
}

/// Empty editor for creating a new script.
pub async fn editor_new(Query(p): Query<EditorParams>) -> Result<Html<String>, AppError> {
    Ok(Html(ScriptEditor {
        id:       String::new(),
        name:     String::new(),
        schedule: String::new(),
        body:     String::new(),
        flash:    p.flash.clone(),
        has_flash: !p.flash.is_empty(),
        is_new:   true,
    }.render()?))
}

/// Editor pre-populated with an existing script.
pub async fn editor_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(p): Query<EditorParams>,
) -> Result<Html<String>, AppError> {
    let resp = rpc(&state, "v2/script", json!({
        "session": SESSION,
        "id":      id,
    })).await?;

    let body = resp.get("script").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let meta = resp.get("metadata").cloned().unwrap_or(json!({}));
    let name = meta.get("name").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let schedule = meta.get("schedule").and_then(|v| v.as_str()).unwrap_or("").to_owned();

    Ok(Html(ScriptEditor {
        id,
        name,
        schedule,
        body,
        flash: p.flash.clone(),
        has_flash: !p.flash.is_empty(),
        is_new: false,
    }.render()?))
}

// ── Save (POST) — create or update ────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SaveForm {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub schedule: String,
    #[serde(default)]
    pub script: String,
}

pub async fn save(
    State(state): State<AppState>,
    Form(form): Form<SaveForm>,
) -> Result<Html<String>, AppError> {
    let metadata = json!({
        "name":     form.name,
        "schedule": form.schedule,
    });

    let (saved_id, flash) = if form.id.is_empty() {
        let resp = rpc(&state, "v2/script_add", json!({
            "session":  SESSION,
            "metadata": metadata,
            "script":   form.script,
        })).await?;
        let id = resp.get("id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        (id, "Created.".to_owned())
    } else {
        rpc(&state, "v2/script_update", json!({
            "session":  SESSION,
            "id":       form.id.clone(),
            "metadata": metadata,
            "script":   form.script,
        })).await?;
        (form.id.clone(), "Saved.".to_owned())
    };

    // Re-render the editor populated with saved fields (so the user keeps
    // editing) and refresh the list pane via HX-Trigger on the response.
    let editor = ScriptEditor {
        id:       saved_id,
        name:     form.name,
        schedule: form.schedule,
        body:     form.script,
        flash,
        has_flash: true,
        is_new: false,
    };
    Ok(Html(editor.render()?))
}

// ── Delete ────────────────────────────────────────────────────────────────────

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    rpc(&state, "v2/script_delete", json!({
        "session": SESSION,
        "id":      id,
    })).await?;

    // Return an empty editor with a flash message.
    Ok(Html(ScriptEditor {
        id:       String::new(),
        name:     String::new(),
        schedule: String::new(),
        body:     String::new(),
        flash:    "Deleted.".to_owned(),
        has_flash: true,
        is_new: true,
    }.render()?))
}
