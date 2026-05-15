use askama::Template;
use axum::{extract::State, response::Html};
use serde_json::{json, Value};

use crate::{
    client::{fmt_ts, rpc, str_val, u64_val},
    error::AppError,
    security::json_for_script,
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
    /// True once the process has been up long enough that an empty
    /// cache means the backend is probably unreachable, not just
    /// slow — the partial then shows an escalated message (L1).
    escalated: bool,
}

// ── Data partial (rendered from a snapshot) ───────────────────────────────────

#[derive(Debug)]
pub struct ShardRow {
    pub label:           String,
    pub primary_count:   u64,
    pub secondary_count: u64,
}

#[derive(Debug)]
pub struct RecentRow {
    pub id:           String,
    pub short_id:     String,
    pub age_secs:     u64,
    pub submitted_at: String,
}

#[derive(Debug)]
pub struct RunningRow {
    pub worker:    u64,
    pub id:        String,
    pub short_id:  String,
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
    embedding_model:      String,
    // ── BUND runtime stats (formerly on /bund) ──────────────────────────────
    n_results:            u64,
    n_bunds:              u64,
    n_recent:             usize,
    n_running:            usize,
    recent_scripts:       Vec<RecentRow>,
    running_scripts:      Vec<RunningRow>,
    has_recent:           bool,
    has_running:          bool,
    refresh_secs:         u64,
    // ── Synthetic-data warning banner (dev/demo only) ──────────────────────
    /// When true, this node has `generate_realistic_data` enabled.
    /// The template renders a prominent red "SYNTHETIC DATA" banner
    /// at the top of the page so operators can never mistake a demo
    /// run for real telemetry.  Populated from `v2/status.dev_data`.
    dev_data_enabled:        bool,
    dev_data_records:        u64,
    dev_data_batches:        u64,
    dev_data_interval_secs:  u64,
    dev_data_total_per_batch: u64,
    // ── Perf headline (from v2/status.perf) ────────────────────────────────
    /// True only when there's been at least one ingest flush — keeps the
    /// tile hidden on a brand-new node before any work has happened.
    perf_has_samples:        bool,
    perf_ingest_flush_p50:   u64,   // ms
    perf_ingest_flush_p95:   u64,   // ms
    perf_ingest_flush_p99:   u64,   // ms
    perf_ingest_flush_n:     u64,
    perf_ingest_lag_p95:     u64,   // ms
    perf_fanout_p95:         u64,   // ms
    perf_replicate_p95:      u64,   // ms
    /// Non-empty when one or more sections are showing carried-over
    /// data because their RPC failed on the last poll (M3).  The
    /// template renders it as an amber "partial data" banner.
    stale_note:              String,
}

fn short_uuid(s: &str) -> String {
    s.split('-').take(2).collect::<Vec<_>>().join("-")
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

    // v2/shards is ordered start_ts ASC, so the `recent` slice runs
    // oldest→newest.  The bar chart wants that (time flows left→right),
    // but the "5 Most Recent" table should lead with the newest row.
    shards.reverse();

    // ── BUND runtime stats from v2/status ──────────────────────────────────
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let recent_scripts: Vec<RecentRow> = snap.status.get("recent_scripts")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(|v| {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_owned();
            let submitted_at = v.get("submitted_at").and_then(|x| x.as_u64()).unwrap_or(0);
            RecentRow {
                short_id:     short_uuid(&id),
                id:           id.clone(),
                age_secs:     now_secs.saturating_sub(submitted_at),
                submitted_at: fmt_ts(submitted_at),
            }
        }).collect())
        .unwrap_or_default();

    let running_scripts: Vec<RunningRow> = snap.status.get("running_scripts")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(|v| {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_owned();
            RunningRow {
                worker:   v.get("worker").and_then(|x| x.as_u64()).unwrap_or(0),
                short_id: short_uuid(&id),
                id,
            }
        }).collect())
        .unwrap_or_default();

    let n_recent  = recent_scripts.len();
    let n_running = running_scripts.len();
    let has_recent  = n_recent  > 0;
    let has_running = n_running > 0;

    // ── Synthetic-data state ─────────────────────────────────────────────
    let dev_data = snap.status.get("dev_data");
    let dev_data_enabled = dev_data
        .and_then(|d| d.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let dev_data_records = dev_data
        .and_then(|d| d.get("records_lifetime"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let dev_data_batches = dev_data
        .and_then(|d| d.get("batches_emitted"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let dev_data_interval_secs = dev_data
        .and_then(|d| d.get("interval_secs"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let dev_data_total_per_batch = dev_data
        .and_then(|d| d.get("total_per_batch"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // ── Perf headline (from v2/status.perf) ──────────────────────────────
    let perf = snap.status.get("perf");
    let perf_get_us = |k: &str| -> u64 {
        perf.and_then(|p| p.get(k)).and_then(|v| v.as_u64()).unwrap_or(0)
    };
    let perf_ingest_flush_n = perf_get_us("ingest_flush_n_total");
    let perf_has_samples = perf_ingest_flush_n > 0;
    // Convert µs → ms for display (one decimal of precision is overkill
    // at the dashboard level; rounded ms is the right call).
    let us_to_ms = |us: u64| us / 1_000;

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
        shard_labels_json:    json_for_script(&labels)?,
        shard_primary_json:   json_for_script(&primary_cnts)?,
        shard_secondary_json: json_for_script(&secondary_cnts)?,
        jsoncache_pct:        u64_val(&snap.status, "jsoncache_pct"),
        jsoncache_len:        u64_val(&snap.status, "jsoncache_len"),
        jsoncache_capacity:   u64_val(&snap.status, "jsoncache_capacity"),
        embedding_model:      str_val(&snap.status, "embedding_model"),
        n_results:            u64_val(&snap.status, "n_results"),
        n_bunds:              u64_val(&snap.status, "n_bunds"),
        n_recent,
        n_running,
        recent_scripts,
        running_scripts,
        has_recent,
        has_running,
        refresh_secs,
        dev_data_enabled,
        dev_data_records,
        dev_data_batches,
        dev_data_interval_secs,
        dev_data_total_per_batch,
        perf_has_samples,
        perf_ingest_flush_p50: us_to_ms(perf_get_us("ingest_flush_p50_us")),
        perf_ingest_flush_p95: us_to_ms(perf_get_us("ingest_flush_p95_us")),
        perf_ingest_flush_p99: us_to_ms(perf_get_us("ingest_flush_p99_us")),
        perf_ingest_flush_n,
        perf_ingest_lag_p95:   us_to_ms(perf_get_us("ingest_lag_p95_us")),
        perf_fanout_p95:       us_to_ms(perf_get_us("fanout_p95_us_max")),
        perf_replicate_p95:    us_to_ms(perf_get_us("replicate_p95_us_max")),
        stale_note: if snap.stale.is_empty() {
            String::new()
        } else {
            format!(
                "Showing last-known data for {} — its latest refresh from bdsnode failed.",
                snap.stale.join(", "),
            )
        },
    };

    Ok(tmpl.render()?)
}

// ── Cached fetch (background-collected by the poller) ────────────────────────

/// Fetches the four dashboard RPCs concurrently from bdsnode and returns the
/// snapshot.  Used by both the live `/dashboard/refresh` handler and the
/// background poller spawned in `main`.
///
/// Uses `join!` (not `try_join!`) so a single failing RPC degrades that
/// one section to its last-known value instead of blanking the whole
/// dashboard (resilience finding M3).  Only when *every* section fails
/// AND there is no prior snapshot does this return `Err` — that keeps
/// the page on its "Wait" placeholder.
pub async fn collect(state: &AppState) -> Result<DashboardSnapshot, AppError> {
    let (status, count, timeline, shards) = tokio::join!(
        rpc(state, "v2/status",   json!({})),
        rpc(state, "v2/count",    json!({})),
        rpc(state, "v2/timeline", json!({})),
        rpc(state, "v2/shards",   json!({})),
    );

    // Previous snapshot — a failed section carries its last-good value
    // forward.  Reading the cache here costs one clone per poll cycle
    // (every `dashboard_refresh_secs`), not per request.
    let prev = state.dashboard_cache.read().await.clone();
    let mut stale: Vec<String> = Vec::new();

    fn section(
        name:  &str,
        res:   Result<Value, AppError>,
        prev:  Option<Value>,
        stale: &mut Vec<String>,
    ) -> Option<Value> {
        match res {
            Ok(v) => Some(v),
            Err(e) => {
                log::warn!("dashboard: {name} failed ({e}) — keeping last-known value");
                stale.push(name.to_owned());
                prev
            }
        }
    }

    let status   = section("v2/status",   status,   prev.as_ref().map(|p| p.status.clone()),   &mut stale);
    let count    = section("v2/count",    count,    prev.as_ref().map(|p| p.count.clone()),    &mut stale);
    let timeline = section("v2/timeline", timeline, prev.as_ref().map(|p| p.timeline.clone()), &mut stale);
    let shards   = section("v2/shards",   shards,   prev.as_ref().map(|p| p.shards.clone()),   &mut stale);

    // Every RPC failed and there was nothing cached to fall back on.
    if status.is_none() && count.is_none() && timeline.is_none() && shards.is_none() {
        return Err(AppError::Msg(
            "dashboard: all backend RPCs failed and no cached data is available".to_owned(),
        ));
    }

    Ok(DashboardSnapshot {
        status:   status.unwrap_or(Value::Null),
        count:    count.unwrap_or(Value::Null),
        timeline: timeline.unwrap_or(Value::Null),
        shards:   shards.unwrap_or(Value::Null),
        stale,
    })
}

/// Renders the dashboard from the cached snapshot.  If the background poller
/// hasn't populated the cache yet, returns a "Wait" partial that auto-refreshes
/// every 2 seconds.
pub async fn data(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    // Hold the read guard across the synchronous render instead of
    // cloning the whole snapshot on every poll (P3).
    let guard = state.dashboard_cache.read().await;
    match &*guard {
        Some(snap) => Ok(Html(render_snapshot(snap, state.dashboard_refresh_secs)?)),
        None => {
            drop(guard);
            // L1: once we've been up a while with still-empty cache,
            // the backend is probably unreachable, not just slow.
            let escalated = state.started_at.elapsed() > std::time::Duration::from_secs(30);
            Ok(Html(DashboardWait {
                poll_url: "/dashboard/data".to_owned(),
                message:  "Background poller is collecting telemetry…".to_owned(),
                escalated,
            }.render()?))
        }
    }
}

/// Forces a live fetch from bdsnode, overwrites the cache, and renders.  The
/// "Reload" button on the dashboard targets this endpoint.
pub async fn refresh(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let snap = collect(&state).await?;
    *state.dashboard_cache.write().await = Some(snap.clone());
    Ok(Html(render_snapshot(&snap, state.dashboard_refresh_secs)?))
}
