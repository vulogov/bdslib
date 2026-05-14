//! `/cluster` — peer-table view backed by `v2/cluster.peers`, mirroring
//! the Dashboard pattern: a thin shell on first load (`page`), a
//! cached partial (`data`) backed by a background poller in `main`,
//! and a `refresh` endpoint that force-fetches.  Auto-refresh
//! interval is configured by `cluster_refresh_secs` in `bds.hjson`.
//!
//! Renders a friendly "cluster mode disabled" panel when the node's
//! `bds.hjson` has `cluster.enabled = false` (or the block is absent).

use askama::Template;
use axum::{extract::State, response::Html};
use serde_json::{json, Value};

use crate::{
    client::{rpc, fmt_ts},
    error::AppError,
    state::{AppState, ClusterSnapshot},
};

#[derive(Debug)]
pub struct PeerRow {
    pub node_id:         String,
    pub short_id:        String,
    pub url:             String,
    pub state:           String,
    pub state_class:     String,
    pub last_seen:       String,
    pub age_secs:        u64,
    pub version:         String,
    pub embedding_model: String,
    pub miss_count:      u64,
    pub hints:           u64,
}

fn short_uuid(s: &str) -> String {
    s.split('-').take(2).collect::<Vec<_>>().join("-")
}

fn state_class(state: &str) -> &'static str {
    match state {
        "alive"   => "text-emerald-400",
        "suspect" => "text-yellow-400",
        "dead"    => "text-red-400",
        _         => "text-slate-400",
    }
}

fn mode_class(mode: &str) -> &'static str {
    match mode {
        "full"       => "text-emerald-400",
        "partial"    => "text-yellow-400",
        "standalone" => "text-slate-400",
        _            => "text-slate-400",
    }
}

// ── Shell (instant) ───────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "cluster.html")]
struct ClusterShell {
    refresh_secs: u64,
}

/// Returns the page skeleton immediately.  HTMX fires `/cluster/data`
/// on load and re-fetches every `cluster_refresh_secs` so the table
/// picks up the background-collected snapshot.
pub async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    Ok(Html(ClusterShell {
        refresh_secs: state.cluster_refresh_secs,
    }.render()?))
}

// ── Wait partial ──────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/cluster_wait.html")]
struct ClusterWait {
    poll_url: String,
    message:  String,
}

// ── Data partial (rendered from a snapshot) ───────────────────────────────────

#[derive(Template)]
#[template(path = "partials/cluster_data.html")]
struct ClusterData {
    enabled:                  bool,
    node_id:                  String,
    bind_url:                 String,
    mode:                     String,
    mode_class:               String,
    alive:                    u64,
    suspect:                  u64,
    dead:                     u64,
    full_mode_threshold:      u64,
    replication_factor:       u64,
    embedding_model:          String,
    uptime_secs:              u64,
    hint_backlog:             u64,
    tombstone_total:          u64,
    last_hint_tick_age_secs:  u64,
    last_hint_tick_replayed:  u64,
    last_ae_tick_age_secs:    u64,
    last_ae_tick_pulled:      u64,
    last_ae_tick_tombstones:  u64,
    last_ae_tick_pruned:      u64,
    has_hint_tick:            bool,
    has_ae_tick:              bool,
    peers:                    Vec<PeerRow>,
    has_peers:                bool,
}

fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn parse_peers(arr: &[Value]) -> Vec<PeerRow> {
    let now = now_secs();
    arr.iter().map(|p| {
        let node_id   = p.get("node_id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let url       = p.get("url").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let state     = p.get("state").and_then(|v| v.as_str()).unwrap_or("dead").to_owned();
        let last_seen = p.get("last_seen").and_then(|v| v.as_u64()).unwrap_or(0);
        let version   = p.get("version").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let embedding = p.get("embedding_model").and_then(|v| v.as_str()).unwrap_or("—").to_owned();
        let _started  = p.get("started_at").and_then(|v| v.as_u64()).unwrap_or(0);
        let miss      = p.get("miss_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let hints     = p.get("hints").and_then(|v| v.as_u64()).unwrap_or(0);

        PeerRow {
            short_id:        short_uuid(&node_id),
            node_id,
            url,
            state_class:     state_class(&state).to_owned(),
            age_secs:        if last_seen == 0 { 0 } else { now.saturating_sub(last_seen) },
            last_seen:       fmt_ts(last_seen),
            state,
            version,
            embedding_model: embedding,
            miss_count:      miss,
            hints,
        }
    }).collect()
}

fn render_snapshot(snap: &ClusterSnapshot) -> Result<String, AppError> {
    let resp = &snap.peers;

    let enabled = resp.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let peers   = resp.get("peers").and_then(|v| v.as_array())
        .map(|a| parse_peers(a))
        .unwrap_or_default();
    let has_peers = !peers.is_empty();

    let stats = resp.get("stats").cloned().unwrap_or(json!({}));
    let now = now_secs();
    let last_hint_tick           = stats.get("last_hint_tick").and_then(|v| v.as_u64()).unwrap_or(0);
    let last_hint_tick_replayed  = stats.get("last_hint_tick_replayed").and_then(|v| v.as_u64()).unwrap_or(0);
    let last_ae_tick             = stats.get("last_ae_tick").and_then(|v| v.as_u64()).unwrap_or(0);
    let last_ae_tick_pulled      = stats.get("last_ae_tick_pulled").and_then(|v| v.as_u64()).unwrap_or(0);
    let last_ae_tick_tombstones  = stats.get("last_ae_tick_tombstones").and_then(|v| v.as_u64()).unwrap_or(0);
    let last_ae_tick_pruned      = stats.get("last_ae_tick_pruned").and_then(|v| v.as_u64()).unwrap_or(0);

    let tmpl = ClusterData {
        enabled,
        node_id:             resp.get("node_id").and_then(|v| v.as_str()).unwrap_or("—").to_owned(),
        bind_url:            resp.get("bind_url").and_then(|v| v.as_str()).unwrap_or("—").to_owned(),
        mode:                resp.get("mode").and_then(|v| v.as_str()).unwrap_or("disabled").to_owned(),
        mode_class:          mode_class(resp.get("mode").and_then(|v| v.as_str()).unwrap_or("")).to_owned(),
        alive:               resp.get("alive").and_then(|v| v.as_u64()).unwrap_or(0),
        suspect:             resp.get("suspect").and_then(|v| v.as_u64()).unwrap_or(0),
        dead:                resp.get("dead").and_then(|v| v.as_u64()).unwrap_or(0),
        full_mode_threshold: resp.get("full_mode_threshold").and_then(|v| v.as_u64()).unwrap_or(3),
        replication_factor:  resp.get("replication_factor").and_then(|v| v.as_u64()).unwrap_or(3),
        embedding_model:     resp.get("embedding_model").and_then(|v| v.as_str()).unwrap_or("—").to_owned(),
        uptime_secs:         resp.get("uptime_secs").and_then(|v| v.as_u64()).unwrap_or(0),
        hint_backlog:        resp.get("hint_backlog").and_then(|v| v.as_u64()).unwrap_or(0),
        tombstone_total:     resp.get("tombstone_total").and_then(|v| v.as_u64()).unwrap_or(0),
        has_hint_tick:           last_hint_tick > 0,
        has_ae_tick:             last_ae_tick > 0,
        last_hint_tick_age_secs: if last_hint_tick > 0 { now.saturating_sub(last_hint_tick) } else { 0 },
        last_hint_tick_replayed,
        last_ae_tick_age_secs:   if last_ae_tick > 0 { now.saturating_sub(last_ae_tick) } else { 0 },
        last_ae_tick_pulled,
        last_ae_tick_tombstones,
        last_ae_tick_pruned,
        peers,
        has_peers,
    };
    Ok(tmpl.render()?)
}

// ── Cached fetch (background-collected by the poller) ────────────────────────

/// Fetches v2/cluster.peers from bdsnode and wraps it in a
/// `ClusterSnapshot`.  Used by both `/cluster/refresh` and the
/// background poller spawned in `main`.
pub async fn collect(state: &AppState) -> Result<ClusterSnapshot, AppError> {
    let peers = rpc(state, "v2/cluster.peers", json!({})).await?;
    Ok(ClusterSnapshot { peers })
}

/// Renders the cluster panel from the cached snapshot.  If the
/// background poller hasn't populated the cache yet, returns a "Wait"
/// partial that auto-refreshes every 2 seconds (same UX as the
/// dashboard).
pub async fn data(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    // Hold the read guard across the synchronous render instead of
    // cloning the whole snapshot on every poll (P3).
    let guard = state.cluster_cache.read().await;
    match &*guard {
        Some(snap) => Ok(Html(render_snapshot(snap)?)),
        None       => {
            drop(guard);
            Ok(Html(ClusterWait {
                poll_url: "/cluster/data".to_owned(),
                message:  "Background poller is fetching cluster state…".to_owned(),
            }.render()?))
        }
    }
}

/// Forces a live fetch from bdsnode, overwrites the cache, and
/// renders.  The "Reload" button on the cluster page targets this
/// endpoint.
pub async fn refresh(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let snap = collect(&state).await?;
    *state.cluster_cache.write().await = Some(snap.clone());
    Ok(Html(render_snapshot(&snap)?))
}
