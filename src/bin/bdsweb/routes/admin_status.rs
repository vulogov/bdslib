//! Administration → Status page.
//!
//! Full operator-grade dashboard over the entire `v2/status` payload —
//! everything the regular Dashboard hides on a single page, plus the
//! pieces the regular Dashboard never showed (pool checkout timeouts,
//! self-healing tracker, per-counter rebalancer + retention details).
//!
//! Direct one-shot RPC per refresh (no background poller cache) —
//! admin traffic is low and we want the page to reflect the most
//! recent counter values, not a cache-window-old snapshot.
//!
//! Mirrors the routing shape of `routes::dashboard`: a thin `page()`
//! shell renders the chrome + an HTMX `hx-trigger="load, every Ns"`
//! placeholder; `data()` is the partial the placeholder polls.

use askama::Template;
use axum::{extract::State, response::Html};
use serde_json::Value;

use crate::{
    client::{fmt_ts, rpc, str_val, u64_val},
    error::AppError,
    state::AppState,
};

// ── Shell ────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "admin_status.html")]
struct AdminStatusShell {
    refresh_secs: u64,
    node_url:     String,
}

pub async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    Ok(Html(AdminStatusShell {
        refresh_secs: state.dashboard_refresh_secs,
        node_url:     (*state.node_url).clone(),
    }.render()?))
}

// ── Live data partial ────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "partials/admin_status_data.html")]
struct AdminStatusData {
    // ─ header strip
    node_id:           String,
    short_node_id:     String,
    hostname:          String,
    version:           String,
    uptime:            String,
    embedding_model:   String,
    timestamp:         String,
    // ─ health
    health_status:        String,
    health_status_class:  &'static str,
    health_reason:        String,
    health_n_sources:     u64,
    health_n_degraded:    u64,
    health_n_failed:      u64,
    // ─ cluster
    cluster_present:        bool,
    cluster_node_id:        String,
    cluster_short_node_id:  String,
    cluster_mode:           String,
    cluster_mode_class:     &'static str,
    cluster_bind_url:       String,
    cluster_alive:          u64,
    cluster_suspect:        u64,
    cluster_dead:           u64,
    cluster_replication_factor: u64,
    cluster_full_mode_threshold: u64,
    cluster_hint_backlog:   u64,
    // ─ ingest queues + flushers
    logs_queue:            u64,
    json_file_queue:       u64,
    json_file_name:        String,
    syslog_file_queue:     u64,
    syslog_file_name:      String,
    flushers_alive:        u64,
    flushers_configured:   u64,
    flushers_restarts:     u64,
    flushers_dropped:      u64,
    flushers_class:        &'static str,
    // ─ JSON cache
    jsoncache_pct:      u64,
    jsoncache_len:      u64,
    jsoncache_capacity: u64,
    // ─ pool
    pool_checkout_timeouts: u64,
    pool_class:             &'static str,
    // ─ perf (everything in ms; raw µs available as title attr in template)
    perf_ingest_flush_p50_ms: u64,
    perf_ingest_flush_p95_ms: u64,
    perf_ingest_flush_p99_ms: u64,
    perf_ingest_flush_n:      u64,
    perf_ingest_lag_p50_ms:   u64,
    perf_ingest_lag_p95_ms:   u64,
    perf_fanout_p95_ms:       u64,
    perf_replicate_p95_ms:    u64,
    perf_has_samples:         bool,
    // ─ rebalancer
    reb_records_replicated_lifetime: u64,
    reb_records_replicated_last_run: u64,
    reb_records_examined_lifetime:   u64,
    reb_records_examined_last_run:   u64,
    reb_batches_examined_lifetime:   u64,
    reb_batches_examined_last_run:   u64,
    reb_paused_for_lag_lifetime:     u64,
    reb_errors_lifetime:             u64,
    reb_last_run:                    String,
    reb_last_run_ms:                 u64,
    // ─ retention
    ret_evicted_lifetime:        u64,
    ret_evicted_last_run:        u64,
    ret_freed_lifetime_bytes:    String,
    ret_freed_last_run_bytes:    String,
    ret_errors_lifetime:         u64,
    ret_quorum_skipped_lifetime: u64,
    ret_quorum_skipped_last_run: u64,
    ret_last_run:                String,
    ret_last_run_ms:             u64,
    // ─ self-healing
    sh_quarantined_now:     u64,
    sh_quarantines_total:   u64,
    sh_heals_total:         u64,
    sh_unhealable_total:    u64,
    sh_recreations_total:   u64,
    sh_breaker_trips_total: u64,
    sh_class:               &'static str,
    // ─ dev_data
    dev_data_enabled:           bool,
    dev_data_config_enabled:    bool,
    dev_data_records_lifetime:  u64,
    dev_data_records_last_batch:u64,
    dev_data_batches_emitted:   u64,
    dev_data_last_run:          String,
    dev_data_last_run_ms:       u64,
    dev_data_errors_lifetime:   u64,
    dev_data_interval_secs:     u64,
    dev_data_duration:          String,
    dev_data_total_per_batch:   u64,
    dev_data_scenarios:         u64,
    dev_data_noise_ratio:       String,
    dev_data_anomaly_ratio:     String,
    dev_data_seed:              String,
    // ─ BUND runtime
    n_results:        u64,
    n_bunds:          u64,
    n_recent:         usize,
    n_running:        usize,
    recent_scripts:   Vec<RecentRow>,
    running_scripts:  Vec<RunningRow>,
    // ─ raw JSON (collapsible, end of page)
    raw_json:         String,
    // ─ error (when v2/status itself fails)
    error_msg:        String,
    has_error:        bool,
}

pub struct RecentRow {
    pub short_id:     String,
    pub id:           String,
    pub submitted_at: String,
}

pub struct RunningRow {
    pub worker:   u64,
    pub short_id: String,
    pub id:       String,
}

pub async fn data(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    match rpc(&state, "v2/status", Value::Null).await {
        Ok(v)  => Ok(Html(build(&v).render()?)),
        Err(e) => {
            // Render an error banner inside the partial rather than 500ing
            // — the HTMX target swaps cleanly and the operator sees the
            // failure inline next to a "Reload" button.
            let mut d = build(&Value::Object(serde_json::Map::new()));
            d.has_error = true;
            d.error_msg = format!("v2/status failed: {e}");
            Ok(Html(d.render()?))
        }
    }
}

// ── Field extraction ─────────────────────────────────────────────────────────

fn build(v: &Value) -> AdminStatusData {
    let cluster   = v.get("cluster");
    let health    = v.get("health");
    let flushers  = v.get("ingest_flushers");
    let pool      = v.get("pool");
    let perf      = v.get("perf");
    let rebal     = v.get("rebalancer");
    let retent    = v.get("retention");
    let sh        = v.get("self_healing");
    let dev_data  = v.get("dev_data");

    let node_id = str_val(v, "node_id");
    let cluster_node_id = cluster.map(|c| str_val(c, "node_id")).unwrap_or_default();
    let cluster_mode = cluster.map(|c| str_val(c, "mode")).unwrap_or_default();

    let perf_ingest_flush_n = perf.map(|p| u64_val(p, "ingest_flush_n_total")).unwrap_or(0);
    let perf_has_samples = perf_ingest_flush_n > 0;

    let us_to_ms = |u: u64| u / 1_000;

    let n_recent  = v.get("recent_scripts").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);
    let n_running = v.get("running_scripts").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0);

    let recent_scripts: Vec<RecentRow> = v.get("recent_scripts")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().take(8).map(|r| {
            let id = str_val(r, "id");
            RecentRow {
                short_id:     short_uuid(&id),
                id,
                submitted_at: fmt_ts(u64_val(r, "submitted_at")),
            }
        }).collect())
        .unwrap_or_default();

    let running_scripts: Vec<RunningRow> = v.get("running_scripts")
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().take(8).map(|r| {
            let id = str_val(r, "id");
            RunningRow {
                worker:   u64_val(r, "worker"),
                short_id: short_uuid(&id),
                id,
            }
        }).collect())
        .unwrap_or_default();

    let flushers_alive       = flushers.map(|f| u64_val(f, "alive")).unwrap_or(0);
    let flushers_configured  = flushers.map(|f| u64_val(f, "configured")).unwrap_or(0);
    let flushers_dropped     = flushers.map(|f| u64_val(f, "records_dropped")).unwrap_or(0);
    let flushers_class = if flushers_alive < flushers_configured || flushers_dropped > 0 {
        "text-amber-300"
    } else {
        "text-emerald-300"
    };

    let pool_checkout_timeouts = pool.map(|p| u64_val(p, "checkout_timeouts")).unwrap_or(0);
    let pool_class = if pool_checkout_timeouts > 0 { "text-amber-300" } else { "text-emerald-300" };

    let sh_quarantined_now    = sh.map(|s| u64_val(s, "quarantined_now")).unwrap_or(0);
    let sh_unhealable_total   = sh.map(|s| u64_val(s, "unhealable_total")).unwrap_or(0);
    let sh_breaker_trips_total= sh.map(|s| u64_val(s, "breaker_trips_total")).unwrap_or(0);
    let sh_class = if sh_quarantined_now > 0 || sh_unhealable_total > 0 || sh_breaker_trips_total > 0 {
        "text-amber-300"
    } else {
        "text-emerald-300"
    };

    let health_status = health.map(|h| str_val(h, "status")).unwrap_or_default();
    let health_status_class = match health_status.as_str() {
        "healthy"  => "text-emerald-300",
        "degraded" => "text-amber-300",
        "failed"   => "text-red-300",
        _          => "text-slate-300",
    };
    let cluster_mode_class = match cluster_mode.as_str() {
        "full"       => "text-emerald-300",
        "partial"    => "text-amber-300",
        "standalone" => "text-slate-300",
        _            => "text-slate-300",
    };

    // dev_data: noise_ratio / anomaly_ratio come through as JSON floats;
    // keep their full precision (not rounded to 2 dp) but stringify.
    let dev_data_noise_ratio   = dev_data
        .and_then(|d| d.get("noise_ratio"))
        .map(|x| x.to_string())
        .unwrap_or_else(|| "—".to_owned());
    let dev_data_anomaly_ratio = dev_data
        .and_then(|d| d.get("anomaly_ratio"))
        .map(|x| x.to_string())
        .unwrap_or_else(|| "—".to_owned());
    let dev_data_seed = dev_data
        .and_then(|d| d.get("seed"))
        .map(|x| match x {
            Value::Null   => "(random)".to_owned(),
            other          => other.to_string(),
        })
        .unwrap_or_else(|| "(random)".to_owned());

    let raw_json = serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_owned());

    AdminStatusData {
        // header
        short_node_id:   short_uuid(&node_id),
        node_id,
        hostname:        str_val(v, "hostname"),
        version:         str_val(v, "version"),
        uptime:          fmt_uptime(u64_val(v, "uptime_secs")),
        embedding_model: str_val(v, "embedding_model"),
        timestamp:       fmt_ts(u64_val(v, "timestamp")),

        // health
        health_status,
        health_status_class,
        health_reason:    health.map(|h| str_val(h, "reason")).unwrap_or_default(),
        health_n_sources: health.map(|h| u64_val(h, "n_sources")).unwrap_or(0),
        health_n_degraded:health.map(|h| u64_val(h, "n_degraded")).unwrap_or(0),
        health_n_failed:  health.map(|h| u64_val(h, "n_failed")).unwrap_or(0),

        // cluster
        cluster_present:        cluster.map(|c| !c.is_null()).unwrap_or(false),
        cluster_short_node_id:  short_uuid(&cluster_node_id),
        cluster_node_id,
        cluster_mode,
        cluster_mode_class,
        cluster_bind_url:       cluster.map(|c| str_val(c, "bind_url")).unwrap_or_default(),
        cluster_alive:          cluster.map(|c| u64_val(c, "alive")).unwrap_or(0),
        cluster_suspect:        cluster.map(|c| u64_val(c, "suspect")).unwrap_or(0),
        cluster_dead:           cluster.map(|c| u64_val(c, "dead")).unwrap_or(0),
        cluster_replication_factor: cluster.map(|c| u64_val(c, "replication_factor")).unwrap_or(0),
        cluster_full_mode_threshold:cluster.map(|c| u64_val(c, "full_mode_threshold")).unwrap_or(0),
        cluster_hint_backlog:   cluster.map(|c| u64_val(c, "hint_backlog")).unwrap_or(0),

        // ingest queues
        logs_queue:        u64_val(v, "logs_queue"),
        json_file_queue:   u64_val(v, "json_file_queue"),
        json_file_name:    str_val(v, "json_file_name"),
        syslog_file_queue: u64_val(v, "syslog_file_queue"),
        syslog_file_name:  str_val(v, "syslog_file_name"),
        flushers_alive,
        flushers_configured,
        flushers_restarts: flushers.map(|f| u64_val(f, "restarts_total")).unwrap_or(0),
        flushers_dropped,
        flushers_class,

        // json cache
        jsoncache_pct:      u64_val(v, "jsoncache_pct"),
        jsoncache_len:      u64_val(v, "jsoncache_len"),
        jsoncache_capacity: u64_val(v, "jsoncache_capacity"),

        // pool
        pool_checkout_timeouts,
        pool_class,

        // perf (µs → ms)
        perf_ingest_flush_p50_ms: us_to_ms(perf.map(|p| u64_val(p, "ingest_flush_p50_us")).unwrap_or(0)),
        perf_ingest_flush_p95_ms: us_to_ms(perf.map(|p| u64_val(p, "ingest_flush_p95_us")).unwrap_or(0)),
        perf_ingest_flush_p99_ms: us_to_ms(perf.map(|p| u64_val(p, "ingest_flush_p99_us")).unwrap_or(0)),
        perf_ingest_flush_n,
        perf_ingest_lag_p50_ms:   us_to_ms(perf.map(|p| u64_val(p, "ingest_lag_p50_us")).unwrap_or(0)),
        perf_ingest_lag_p95_ms:   us_to_ms(perf.map(|p| u64_val(p, "ingest_lag_p95_us")).unwrap_or(0)),
        perf_fanout_p95_ms:       us_to_ms(perf.map(|p| u64_val(p, "fanout_p95_us_max")).unwrap_or(0)),
        perf_replicate_p95_ms:    us_to_ms(perf.map(|p| u64_val(p, "replicate_p95_us_max")).unwrap_or(0)),
        perf_has_samples,

        // rebalancer
        reb_records_replicated_lifetime: rebal.map(|r| u64_val(r, "records_replicated_lifetime")).unwrap_or(0),
        reb_records_replicated_last_run: rebal.map(|r| u64_val(r, "records_replicated_last_run")).unwrap_or(0),
        reb_records_examined_lifetime:   rebal.map(|r| u64_val(r, "records_examined_lifetime")).unwrap_or(0),
        reb_records_examined_last_run:   rebal.map(|r| u64_val(r, "records_examined_last_run")).unwrap_or(0),
        reb_batches_examined_lifetime:   rebal.map(|r| u64_val(r, "batches_examined_lifetime")).unwrap_or(0),
        reb_batches_examined_last_run:   rebal.map(|r| u64_val(r, "batches_examined_last_run")).unwrap_or(0),
        reb_paused_for_lag_lifetime:     rebal.map(|r| u64_val(r, "paused_for_lag_lifetime")).unwrap_or(0),
        reb_errors_lifetime:             rebal.map(|r| u64_val(r, "errors_lifetime")).unwrap_or(0),
        reb_last_run:                    fmt_ts(rebal.map(|r| u64_val(r, "last_run_ts")).unwrap_or(0)),
        reb_last_run_ms:                 rebal.map(|r| u64_val(r, "last_run_ms")).unwrap_or(0),

        // retention
        ret_evicted_lifetime:        retent.map(|r| u64_val(r, "evicted_lifetime")).unwrap_or(0),
        ret_evicted_last_run:        retent.map(|r| u64_val(r, "evicted_last_run")).unwrap_or(0),
        ret_freed_lifetime_bytes:    fmt_bytes(retent.map(|r| u64_val(r, "freed_lifetime_bytes")).unwrap_or(0)),
        ret_freed_last_run_bytes:    fmt_bytes(retent.map(|r| u64_val(r, "freed_last_run_bytes")).unwrap_or(0)),
        ret_errors_lifetime:         retent.map(|r| u64_val(r, "errors_lifetime")).unwrap_or(0),
        ret_quorum_skipped_lifetime: retent.map(|r| u64_val(r, "quorum_skipped_lifetime")).unwrap_or(0),
        ret_quorum_skipped_last_run: retent.map(|r| u64_val(r, "quorum_skipped_last_run")).unwrap_or(0),
        ret_last_run:                fmt_ts(retent.map(|r| u64_val(r, "last_run_ts")).unwrap_or(0)),
        ret_last_run_ms:             retent.map(|r| u64_val(r, "last_run_ms")).unwrap_or(0),

        // self-healing
        sh_quarantined_now,
        sh_quarantines_total:   sh.map(|s| u64_val(s, "quarantines_total")).unwrap_or(0),
        sh_heals_total:         sh.map(|s| u64_val(s, "heals_total")).unwrap_or(0),
        sh_unhealable_total,
        sh_recreations_total:   sh.map(|s| u64_val(s, "recreations_total")).unwrap_or(0),
        sh_breaker_trips_total,
        sh_class,

        // dev_data
        dev_data_enabled:           dev_data.and_then(|d| d.get("enabled")).and_then(|x| x.as_bool()).unwrap_or(false),
        dev_data_config_enabled:    dev_data.and_then(|d| d.get("config_enabled")).and_then(|x| x.as_bool()).unwrap_or(false),
        dev_data_records_lifetime:  dev_data.map(|d| u64_val(d, "records_lifetime")).unwrap_or(0),
        dev_data_records_last_batch:dev_data.map(|d| u64_val(d, "records_last_batch")).unwrap_or(0),
        dev_data_batches_emitted:   dev_data.map(|d| u64_val(d, "batches_emitted")).unwrap_or(0),
        dev_data_last_run:          fmt_ts(dev_data.map(|d| u64_val(d, "last_run_ts")).unwrap_or(0)),
        dev_data_last_run_ms:       dev_data.map(|d| u64_val(d, "last_run_ms")).unwrap_or(0),
        dev_data_errors_lifetime:   dev_data.map(|d| u64_val(d, "errors_lifetime")).unwrap_or(0),
        dev_data_interval_secs:     dev_data.map(|d| u64_val(d, "interval_secs")).unwrap_or(0),
        dev_data_duration:          dev_data.map(|d| str_val(d, "duration")).unwrap_or_default(),
        dev_data_total_per_batch:   dev_data.map(|d| u64_val(d, "total_per_batch")).unwrap_or(0),
        dev_data_scenarios:         dev_data.map(|d| u64_val(d, "scenarios")).unwrap_or(0),
        dev_data_noise_ratio,
        dev_data_anomaly_ratio,
        dev_data_seed,

        // BUND
        n_results:        u64_val(v, "n_results"),
        n_bunds:          u64_val(v, "n_bunds"),
        n_recent,
        n_running,
        recent_scripts,
        running_scripts,

        // raw + error
        raw_json,
        error_msg: String::new(),
        has_error: false,
    }
}

fn short_uuid(s: &str) -> String {
    s.split('-').take(2).collect::<Vec<_>>().join("-")
}

fn fmt_uptime(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    let s = secs % 60;
    if d > 0      { format!("{d}d {h}h {m}m") }
    else if h > 0 { format!("{h}h {m}m {s}s") }
    else if m > 0 { format!("{m}m {s}s") }
    else          { format!("{s}s") }
}

fn fmt_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB      { format!("{:.2} GiB", n as f64 / GIB as f64) }
    else if n >= MIB { format!("{:.2} MiB", n as f64 / MIB as f64) }
    else if n >= KIB { format!("{:.1} KiB", n as f64 / KIB as f64) }
    else             { format!("{n} B") }
}
