use askama::Template;
use axum::{extract::State, response::Html};
use serde_json::json;

use crate::{
    client::{fmt_ts, rpc, str_val, u64_val},
    error::AppError,
    state::{AppState, DashboardSnapshot},
};

// ── Shell (instant) ───────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardShell {
    refresh_secs: u64,
}

/// Returns the page skeleton immediately — no RPC calls.
/// HTMX fires `/dashboard/data` on load to fetch the actual content,
/// then re-fetches every `refresh_secs` so the UI picks up each new
/// background-collected snapshot.
pub async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    Ok(Html(DashboardShell {
        refresh_secs: state.dashboard_refresh_secs,
    }.render()?))
}

// ── Wait partial ──────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/dashboard_wait.html")]
struct DashboardWait {
    poll_url: String,
    message:  String,
}

// ── Data partial (rendered from a snapshot) ───────────────────────────────────

#[derive(Debug)]
pub struct ShardRow {
    pub label:           String,
    pub primary_count:   u64,
    pub secondary_count: u64,
}

#[derive(Template)]
#[template(path = "partials/dashboard_data.html")]
struct DashboardData {
    node_id:              String,
    hostname:             String,
    uptime_secs:          u64,
    logs_queue:           u64,
    json_file_queue:      u64,
    syslog_file_queue:    u64,
    total_count:          u64,
    min_ts:               String,
    max_ts:               String,
    total_shards:         usize,
    shards:               Vec<ShardRow>,
    shard_labels_json:    String,
    shard_primary_json:   String,
    shard_secondary_json: String,
    jsoncache_pct:        u64,
    jsoncache_len:        u64,
    jsoncache_capacity:   u64,
    refresh_secs:         u64,
}

const RECENT_SHARDS: usize = 5;

fn render_snapshot(snap: &DashboardSnapshot, refresh_secs: u64) -> Result<String, AppError> {
    let shard_arr = snap.shards.as_array().cloned().unwrap_or_default();
    let total_shards = shard_arr.len();

    let recent = if shard_arr.len() > RECENT_SHARDS {
        &shard_arr[shard_arr.len() - RECENT_SHARDS..]
    } else {
        &shard_arr[..]
    };

    let mut shards         = Vec::with_capacity(recent.len());
    let mut labels         = Vec::with_capacity(recent.len());
    let mut primary_cnts   = Vec::with_capacity(recent.len());
    let mut secondary_cnts = Vec::with_capacity(recent.len());

    for s in recent {
        let start = u64_val(s, "start_ts");
        let p     = u64_val(s, "primary_count");
        let sec   = u64_val(s, "secondary_count");
        let label = fmt_ts(start);
        labels.push(label.clone());
        primary_cnts.push(p);
        secondary_cnts.push(sec);
        shards.push(ShardRow { label, primary_count: p, secondary_count: sec });
    }

    let tmpl = DashboardData {
        node_id:              str_val(&snap.status, "node_id"),
        hostname:             str_val(&snap.status, "hostname"),
        uptime_secs:          u64_val(&snap.status, "uptime_secs"),
        logs_queue:           u64_val(&snap.status, "logs_queue"),
        json_file_queue:      u64_val(&snap.status, "json_file_queue"),
        syslog_file_queue:    u64_val(&snap.status, "syslog_file_queue"),
        total_count:          u64_val(&snap.count,    "count"),
        min_ts:               fmt_ts(u64_val(&snap.timeline, "min_ts")),
        max_ts:               fmt_ts(u64_val(&snap.timeline, "max_ts")),
        total_shards,
        shards,
        shard_labels_json:    serde_json::to_string(&labels)?,
        shard_primary_json:   serde_json::to_string(&primary_cnts)?,
        shard_secondary_json: serde_json::to_string(&secondary_cnts)?,
        jsoncache_pct:        u64_val(&snap.status, "jsoncache_pct"),
        jsoncache_len:        u64_val(&snap.status, "jsoncache_len"),
        jsoncache_capacity:   u64_val(&snap.status, "jsoncache_capacity"),
        refresh_secs,
    };

    Ok(tmpl.render()?)
}

// ── Cached fetch (background-collected by the poller) ────────────────────────

/// Fetches the four dashboard RPCs concurrently from bdsnode and returns the
/// snapshot.  Used by both the live `/dashboard/refresh` handler and the
/// background poller spawned in `main`.
pub async fn collect(state: &AppState) -> Result<DashboardSnapshot, AppError> {
    let (status, count, timeline, shards) = tokio::try_join!(
        rpc(state, "v2/status",   json!({})),
        rpc(state, "v2/count",    json!({})),
        rpc(state, "v2/timeline", json!({})),
        rpc(state, "v2/shards",   json!({})),
    )?;
    Ok(DashboardSnapshot { status, count, timeline, shards })
}

/// Renders the dashboard from the cached snapshot.  If the background poller
/// hasn't populated the cache yet, returns a "Wait" partial that auto-refreshes
/// every 2 seconds.
pub async fn data(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let cached = state.dashboard_cache.read().await.clone();
    match cached {
        Some(snap) => Ok(Html(render_snapshot(&snap, state.dashboard_refresh_secs)?)),
        None => Ok(Html(DashboardWait {
            poll_url: "/dashboard/data".to_owned(),
            message:  "Background poller is collecting telemetry…".to_owned(),
        }.render()?)),
    }
}

/// Forces a live fetch from bdsnode, overwrites the cache, and renders.  The
/// "Reload" button on the dashboard targets this endpoint.
pub async fn refresh(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let snap = collect(&state).await?;
    *state.dashboard_cache.write().await = Some(snap.clone());
    Ok(Html(render_snapshot(&snap, state.dashboard_refresh_secs)?))
}
