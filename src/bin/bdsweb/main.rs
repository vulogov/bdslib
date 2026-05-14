mod admin;
mod auth;
mod client;
mod error;
mod error_pretty;
mod markdown;
mod routes;
mod security;
mod state;

use axum::{middleware, routing::{delete, get, post}, Router};
use clap::Parser;
use state::AppState;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_governor::{governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor, GovernorLayer};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;

#[derive(Parser)]
#[command(name = "bdsweb", about = "bdsnode web UI")]
struct Args {
    /// Address to bind the web server
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Port to bind the web server
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// bdsnode JSON-RPC endpoint
    #[arg(short, long, env = "BDSNODE_URL", default_value = "http://127.0.0.1:9000")]
    node: String,

    /// Path to bds.hjson config file (reads dashboard refresh + cluster
    /// shared secret for auth middleware).
    #[arg(short, long, env = "BDS_CONFIG")]
    config: Option<String>,

    /// Log verbosity (0=warn, 1=info, 2=debug)
    #[arg(long, default_value_t = 1)]
    verbose: u8,
}

struct WebConfig {
    dashboard_refresh_secs: u64,
    /// Background-poll interval for the Cluster page (`/cluster`).
    /// Mirrors `dashboard_refresh_secs`'s semantics: the poller fires
    /// `v2/cluster.peers` once per interval and the page reads from a
    /// cached snapshot.  `1` is the floor; defaults to 10 s.
    cluster_refresh_secs:   u64,
    /// Cluster shared secret read from `cluster.shared_secret` in
    /// `bds.hjson`.  Empty when the config file is missing, the
    /// cluster block is absent, or `cluster.enabled = false` — in
    /// all three cases bdsweb runs in open-access mode (no auth
    /// middleware, no login form).
    shared_secret:          String,
    /// `cluster.auth_rate_limit_per_minute` — Phase 6 rate limit
    /// applied per-IP to `POST /login`.  `0` disables the limit.
    auth_rate_limit_per_minute: u32,
    /// `web.trusted_proxy` — when `true`, bdsweb trusts the
    /// `X-Forwarded-For` / `X-Real-IP` headers for the `POST /login`
    /// rate-limiter key (`SmartIpKeyExtractor`).  Leave `false` unless
    /// bdsweb genuinely sits behind a reverse proxy that sets those
    /// headers — otherwise a client can spoof them to evade the limit.
    trusted_proxy: bool,
    /// `web.secure_cookies` — whether the `bds_session` cookie carries
    /// the `Secure` attribute.  `None` (key absent) means "auto":
    /// resolved at startup to `false` for a loopback bind and `true`
    /// otherwise.  An explicit value overrides the heuristic.
    secure_cookies: Option<bool>,
    /// Operator-tunable knobs for Telemetry → Logs → "Analyze this!".
    logs_analyze:           state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Telemetry → Metrics → "Analyze this!".
    metrics_analyze:        state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Telemetry → Templates → "Analyze this!".
    templates_analyze:      state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Analysis → Agg. Search → "Analyze this!".
    agg_search_analyze:     state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Analysis → Templates Summary → "Analyze this!".
    templates_summary_analyze: state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Analysis → Primary Summary → "Analyze this!".
    primary_summary_analyze:   state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Analysis → Primary Query Summary → "Analyze this!".
    primary_query_summary_analyze: state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Analysis → Primary LSA Summary → "Analyze this!".
    primary_lsa_summary_analyze:   state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Analysis → Primary LSA Query Summary → "Analyze this!".
    primary_lsa_query_summary_analyze: state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Analysis → Detect Anomalies → "Analyze this!".
    anomaly_recent_analyze:            state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Analysis → Denoise → "Analyze this!".
    denoise_recent_analyze:            state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Analysis → k-NN → "Analyze this!".
    knn_analyze:                       state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for RCA → Telemetry RCA → "Analyze this!".
    rca_analyze:                       state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for RCA → Templates RCA → "Analyze this!".
    rca_templates_analyze:             state::AnalyzeTargetConfig,
    /// Operator-tunable knobs for Administration → Performance → "Analyze this!".
    perf_analyze:                      state::AnalyzeTargetConfig,
}

/// True when `host` is a loopback bind — a loopback IP (`127.0.0.0/8`,
/// `::1`) or the `localhost` hostname.  `0.0.0.0`, `::`, or any other
/// hostname is treated as non-loopback so the open-access guard (H3)
/// and the auto Secure-cookie heuristic (H2) fail safe.
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

fn load_config(config_path: Option<&str>) -> WebConfig {
    let defaults = WebConfig {
        dashboard_refresh_secs: 30,
        cluster_refresh_secs:   10,
        shared_secret: String::new(),
        auth_rate_limit_per_minute: 10,
        secure_cookies: None,
        trusted_proxy: false,
        logs_analyze:                      state::AnalyzeTargetConfig::logs_default(),
        metrics_analyze:                   state::AnalyzeTargetConfig::metrics_default(),
        templates_analyze:                 state::AnalyzeTargetConfig::templates_default(),
        agg_search_analyze:                state::AnalyzeTargetConfig::agg_search_default(),
        templates_summary_analyze:         state::AnalyzeTargetConfig::templates_summary_default(),
        primary_summary_analyze:           state::AnalyzeTargetConfig::primary_summary_default(),
        primary_query_summary_analyze:     state::AnalyzeTargetConfig::primary_query_summary_default(),
        primary_lsa_summary_analyze:       state::AnalyzeTargetConfig::primary_lsa_summary_default(),
        primary_lsa_query_summary_analyze: state::AnalyzeTargetConfig::primary_lsa_query_summary_default(),
        anomaly_recent_analyze:            state::AnalyzeTargetConfig::anomaly_recent_default(),
        denoise_recent_analyze:            state::AnalyzeTargetConfig::denoise_recent_default(),
        knn_analyze:                       state::AnalyzeTargetConfig::knn_default(),
        rca_analyze:                       state::AnalyzeTargetConfig::rca_default(),
        rca_templates_analyze:             state::AnalyzeTargetConfig::rca_templates_default(),
        perf_analyze:                      state::AnalyzeTargetConfig::perf_default(),
    };
    let path = match config_path {
        Some(p) => p,
        None => return defaults,
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return defaults,
    };
    let val: serde_hjson::Value = match serde_hjson::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return defaults,
    };
    let obj = match val.as_object() {
        Some(o) => o,
        None => return defaults,
    };
    // Cluster block is optional; its absence (or enabled=false)
    // leaves `shared_secret` empty → open-access mode.
    let cluster_block = obj.get("cluster").and_then(|v| v.as_object());
    let cluster_enabled = cluster_block
        .and_then(|c| c.get("enabled").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    let shared_secret = cluster_block
        .filter(|_| cluster_enabled)
        .and_then(|c| c.get("shared_secret").and_then(|v| v.as_str()).map(str::to_owned))
        .unwrap_or_default();
    let auth_rate_limit_per_minute = cluster_block
        .filter(|_| cluster_enabled)
        .and_then(|c| c.get("auth_rate_limit_per_minute").and_then(|v| v.as_f64()))
        .map(|n| n as u32)
        .unwrap_or(defaults.auth_rate_limit_per_minute);
    let secure_cookies = obj.get("web")
        .and_then(|v| v.as_object())
        .and_then(|w| w.get("secure_cookies"))
        .and_then(|v| v.as_bool());
    let trusted_proxy = obj.get("web")
        .and_then(|v| v.as_object())
        .and_then(|w| w.get("trusted_proxy"))
        .and_then(|v| v.as_bool())
        .unwrap_or(defaults.trusted_proxy);

    // `web.analyze.<target>.*` — every key is optional; the
    // per-target default factory supplies the fallback values, so a
    // missing block (or any missing key inside it) keeps the current
    // behaviour.  Adding `rca`, `signals`, … later is one more
    // closure call below, mirroring `logs` and `metrics`.
    let parse_target = |sub: &str, d: state::AnalyzeTargetConfig| -> state::AnalyzeTargetConfig {
        let block = obj.get("web")
            .and_then(|v| v.as_object())
            .and_then(|w| w.get("analyze"))
            .and_then(|v| v.as_object())
            .and_then(|a| a.get(sub))
            .and_then(|v| v.as_object());
        match block {
            Some(b) => state::AnalyzeTargetConfig {
                timeout_secs: b.get("timeout_secs")
                    .and_then(|v| v.as_f64()).map(|n| n as u64)
                    .unwrap_or(d.timeout_secs).max(30),
                max_rows:     b.get("max_rows")
                    .and_then(|v| v.as_f64()).map(|n| n as usize)
                    .unwrap_or(d.max_rows).clamp(1, 500),
                prompt_template: b.get("prompt_template")
                    .and_then(|v| v.as_str()).map(str::to_owned)
                    .unwrap_or(d.prompt_template),
            },
            None => d,
        }
    };
    let logs_analyze                      = parse_target("logs",                      state::AnalyzeTargetConfig::logs_default());
    let metrics_analyze                   = parse_target("metrics",                   state::AnalyzeTargetConfig::metrics_default());
    let templates_analyze                 = parse_target("templates",                 state::AnalyzeTargetConfig::templates_default());
    let agg_search_analyze                = parse_target("agg_search",                state::AnalyzeTargetConfig::agg_search_default());
    let templates_summary_analyze         = parse_target("templates_summary",         state::AnalyzeTargetConfig::templates_summary_default());
    let primary_summary_analyze           = parse_target("primary_summary",           state::AnalyzeTargetConfig::primary_summary_default());
    let primary_query_summary_analyze     = parse_target("primary_query_summary",     state::AnalyzeTargetConfig::primary_query_summary_default());
    let primary_lsa_summary_analyze       = parse_target("primary_lsa_summary",       state::AnalyzeTargetConfig::primary_lsa_summary_default());
    let primary_lsa_query_summary_analyze = parse_target("primary_lsa_query_summary", state::AnalyzeTargetConfig::primary_lsa_query_summary_default());
    let anomaly_recent_analyze            = parse_target("anomaly_recent",            state::AnalyzeTargetConfig::anomaly_recent_default());
    let denoise_recent_analyze            = parse_target("denoise_recent",            state::AnalyzeTargetConfig::denoise_recent_default());
    let knn_analyze                       = parse_target("knn",                       state::AnalyzeTargetConfig::knn_default());
    let rca_analyze                       = parse_target("rca",                       state::AnalyzeTargetConfig::rca_default());
    let rca_templates_analyze             = parse_target("rca_templates",             state::AnalyzeTargetConfig::rca_templates_default());
    let perf_analyze                      = parse_target("perf",                      state::AnalyzeTargetConfig::perf_default());

    WebConfig {
        dashboard_refresh_secs: obj.get("dashboard_refresh_secs")
            .and_then(|v| v.as_f64())
            .map(|n| n as u64)
            .unwrap_or(defaults.dashboard_refresh_secs)
            .max(1),
        cluster_refresh_secs: obj.get("cluster_refresh_secs")
            .and_then(|v| v.as_f64())
            .map(|n| n as u64)
            .unwrap_or(defaults.cluster_refresh_secs)
            .max(1),
        shared_secret,
        auth_rate_limit_per_minute,
        secure_cookies,
        trusted_proxy,
        logs_analyze,
        metrics_analyze,
        templates_analyze,
        agg_search_analyze,
        templates_summary_analyze,
        primary_summary_analyze,
        primary_query_summary_analyze,
        primary_lsa_summary_analyze,
        primary_lsa_query_summary_analyze,
        anomaly_recent_analyze,
        denoise_recent_analyze,
        knn_analyze,
        rca_analyze,
        rca_templates_analyze,
        perf_analyze,
    }
}

/// Spawn a never-ending background task and keep it alive.  `make`
/// produces the task future; if that task ever panics or returns,
/// `supervise` logs it and re-spawns after a short delay.  The poller
/// bodies are infinite loops, so a plain return is itself unexpected.
/// This is the H1 fix: a panic inside a poll iteration no longer
/// silently kills the poller and freezes its cache forever.
fn supervise<F, Fut>(name: &'static str, make: F)
where
    F:   Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match tokio::spawn(make()).await {
                Ok(())                     => log::error!("[{name}] background task exited unexpectedly — restarting in 5s"),
                Err(e) if e.is_panic()     => log::error!("[{name}] background task panicked — restarting in 5s: {e}"),
                Err(e)                     => { log::error!("[{name}] background task cancelled, not restarting: {e}"); break; }
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}

/// Dashboard cache refresher — polls the 4 dashboard RPCs every
/// `dashboard_refresh_secs` and parks the snapshot in the cache.  On a
/// poll error the previous snapshot is kept (graceful degradation).
async fn dashboard_poll_loop(state: AppState) {
    let interval = std::time::Duration::from_secs(state.dashboard_refresh_secs);
    log::info!("dashboard background poller started (interval={}s)", state.dashboard_refresh_secs);
    loop {
        match routes::dashboard::collect(&state).await {
            Ok(snap) => {
                *state.dashboard_cache.write().await = Some(snap);
                // Stamp the last-success time for the /healthz probe (M4).
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                state.dashboard_last_ok.store(now, std::sync::atomic::Ordering::Relaxed);
                log::debug!("dashboard cache refreshed");
            }
            Err(e) => log::warn!("dashboard background poll failed: {e}"),
        }
        tokio::time::sleep(interval).await;
    }
}

/// Cluster cache refresher — same recipe as [`dashboard_poll_loop`] but
/// polls `v2/cluster.peers` for the `/cluster` page.
async fn cluster_poll_loop(state: AppState) {
    let interval = std::time::Duration::from_secs(state.cluster_refresh_secs);
    log::info!("cluster background poller started (interval={}s)", state.cluster_refresh_secs);
    loop {
        match routes::cluster::collect(&state).await {
            Ok(snap) => {
                *state.cluster_cache.write().await = Some(snap);
                log::debug!("cluster cache refreshed");
            }
            Err(e) => log::warn!("cluster background poll failed: {e}"),
        }
        tokio::time::sleep(interval).await;
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let level = match args.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();

    let cfg = load_config(args.config.as_deref());
    let host_is_loopback = is_loopback_host(&args.host);
    if cfg.shared_secret.is_empty() {
        // H3: open-access mode bypasses ALL auth — admin pages and
        // /bund (arbitrary Bund eval) included.  Refuse to expose that
        // surface on a non-loopback interface.
        if !host_is_loopback {
            log::error!(
                "REFUSING TO START: bdsweb is in OPEN-ACCESS mode (no cluster.shared_secret) but \
                 --host {} is not a loopback address. Open-access mode leaves the entire admin + \
                 Bund-eval surface unauthenticated. Bind to 127.0.0.1 for local dev, or configure \
                 cluster.shared_secret to enable session auth.",
                args.host,
            );
            std::process::exit(1);
        }
        log::warn!("============================================================");
        log::warn!("  bdsweb is running in OPEN-ACCESS mode — SESSION AUTH IS OFF");
        log::warn!("  No cluster.shared_secret configured. Every page, including");
        log::warn!("  /admin/* and /bund (arbitrary Bund eval), is unauthenticated.");
        log::warn!("  Bound to loopback ({}) only — do not expose this process.", args.host);
        log::warn!("============================================================");
    } else {
        log::info!("bdsweb auth enabled — shared_secret loaded ({} bytes)", cfg.shared_secret.len());
    }

    // H2: resolve the effective Secure-cookie policy.  An explicit
    // `web.secure_cookies` wins; otherwise default to off for a
    // loopback bind (dev / HTTP-only) and on for any other address.
    let secure_cookies = cfg.secure_cookies.unwrap_or(!host_is_loopback);
    log::info!(
        "bdsweb session cookie Secure flag: {}{}",
        secure_cookies,
        if cfg.secure_cookies.is_some() { " (web.secure_cookies)" } else { " (auto from bind address)" },
    );
    log::info!(
        "web.analyze.logs: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.logs_analyze.timeout_secs,
        cfg.logs_analyze.max_rows,
        cfg.logs_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.metrics: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.metrics_analyze.timeout_secs,
        cfg.metrics_analyze.max_rows,
        cfg.metrics_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.templates: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.templates_analyze.timeout_secs,
        cfg.templates_analyze.max_rows,
        cfg.templates_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.agg_search: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.agg_search_analyze.timeout_secs,
        cfg.agg_search_analyze.max_rows,
        cfg.agg_search_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.templates_summary: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.templates_summary_analyze.timeout_secs,
        cfg.templates_summary_analyze.max_rows,
        cfg.templates_summary_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.primary_summary:  timeout={}s, max_rows={}, prompt_chars={}",
        cfg.primary_summary_analyze.timeout_secs,
        cfg.primary_summary_analyze.max_rows,
        cfg.primary_summary_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.primary_query_summary: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.primary_query_summary_analyze.timeout_secs,
        cfg.primary_query_summary_analyze.max_rows,
        cfg.primary_query_summary_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.primary_lsa_summary: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.primary_lsa_summary_analyze.timeout_secs,
        cfg.primary_lsa_summary_analyze.max_rows,
        cfg.primary_lsa_summary_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.primary_lsa_query_summary: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.primary_lsa_query_summary_analyze.timeout_secs,
        cfg.primary_lsa_query_summary_analyze.max_rows,
        cfg.primary_lsa_query_summary_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.anomaly_recent: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.anomaly_recent_analyze.timeout_secs,
        cfg.anomaly_recent_analyze.max_rows,
        cfg.anomaly_recent_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.denoise_recent: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.denoise_recent_analyze.timeout_secs,
        cfg.denoise_recent_analyze.max_rows,
        cfg.denoise_recent_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.knn: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.knn_analyze.timeout_secs,
        cfg.knn_analyze.max_rows,
        cfg.knn_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.rca: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.rca_analyze.timeout_secs,
        cfg.rca_analyze.max_rows,
        cfg.rca_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.rca_templates: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.rca_templates_analyze.timeout_secs,
        cfg.rca_templates_analyze.max_rows,
        cfg.rca_templates_analyze.prompt_template.len(),
    );
    log::info!(
        "web.analyze.perf: timeout={}s, max_rows={}, prompt_chars={}",
        cfg.perf_analyze.timeout_secs,
        cfg.perf_analyze.max_rows,
        cfg.perf_analyze.prompt_template.len(),
    );
    let state = AppState::new(
        args.node.clone(),
        cfg.dashboard_refresh_secs,
        cfg.cluster_refresh_secs,
        cfg.shared_secret,
        secure_cookies,
        cfg.logs_analyze,
        cfg.metrics_analyze,
        cfg.templates_analyze,
        cfg.agg_search_analyze,
        cfg.templates_summary_analyze,
        cfg.primary_summary_analyze,
        cfg.primary_query_summary_analyze,
        cfg.primary_lsa_summary_analyze,
        cfg.primary_lsa_query_summary_analyze,
        cfg.anomaly_recent_analyze,
        cfg.denoise_recent_analyze,
        cfg.knn_analyze,
        cfg.rca_analyze,
        cfg.rca_templates_analyze,
        cfg.perf_analyze,
    );

    // Background pollers, each kept alive by `supervise`: a panic in a
    // poll iteration (e.g. an unwrap on an unexpected JSON shape) would
    // otherwise kill the spawned task for good and freeze its cache —
    // `supervise` re-spawns it instead (H1).
    {
        let s = state.clone();
        supervise("dashboard-poller", move || dashboard_poll_loop(s.clone()));
    }
    {
        let s = state.clone();
        supervise("cluster-poller", move || cluster_poll_loop(s.clone()));
    }

    let app = Router::new()
        .route("/",                  get(routes::dashboard::page))
        .route("/dashboard/data",    get(routes::dashboard::data))
        .route("/dashboard/refresh", get(routes::dashboard::refresh))
        .route("/cluster",           get(routes::cluster::page))
        .route("/cluster/data",      get(routes::cluster::data))
        .route("/cluster/refresh",   get(routes::cluster::refresh))
        .route("/perf",              get(routes::perf::page))
        .route("/perf/data",         get(routes::perf::data))
        .route("/perf/analyze",      post(routes::perf::analyze))
        .route("/telemetry",         get(routes::telemetry::page))
        .route("/telemetry/results", get(routes::telemetry::results))
        .route("/telemetry/keys",    get(routes::telemetry::keys))
        .route("/telemetry/analyze", post(routes::telemetry::analyze))
        .route("/logs",              get(routes::logs::page))
        .route("/logs/results",      get(routes::logs::results))
        .route("/logs/keys",         get(routes::logs::keys))
        .route("/logs/topics",       get(routes::logs::topics))
        .route("/logs/analyze",      post(routes::logs::analyze))
        .route("/docs",           get(routes::docs::page))
        .route("/docs/results",   get(routes::docs::results))
        .route("/search",         get(routes::search::page))
        .route("/search/results", get(routes::search::results))
        .route("/search/analyze", post(routes::search::analyze))
        .route("/trends",           get(routes::trends::page))
        .route("/trends/results",   get(routes::trends::results))
        .route("/signals",          get(routes::signals::page))
        .route("/signals/results",  get(routes::signals::results))
        .route("/rca",              get(routes::rca::page))
        .route("/rca/results",    get(routes::rca::results))
        .route("/rca/analyze",    post(routes::rca::analyze))
        .route("/rca/templates",         get(routes::rca_templates::page))
        .route("/rca/templates/results", get(routes::rca_templates::results))
        .route("/rca/templates/analyze", post(routes::rca_templates::analyze))
        .route("/templates",         get(routes::templates::page))
        .route("/templates/results", get(routes::templates::results))
        .route("/templates/analyze", post(routes::templates::analyze))
        .route("/templates_summary",         get(routes::templates_summary::page))
        .route("/templates_summary/results", get(routes::templates_summary::results))
        .route("/templates_summary/analyze", post(routes::templates_summary::analyze))
        .route("/primary_summary",         get(routes::primary_summary::page))
        .route("/primary_summary/results", get(routes::primary_summary::results))
        .route("/primary_summary/analyze", post(routes::primary_summary::analyze))
        .route("/primary_query_summary",         get(routes::primary_query_summary::page))
        .route("/primary_query_summary/results", get(routes::primary_query_summary::results))
        .route("/primary_query_summary/analyze", post(routes::primary_query_summary::analyze))
        .route("/primary_lsa_summary",         get(routes::primary_lsa_summary::page))
        .route("/primary_lsa_summary/results", get(routes::primary_lsa_summary::results))
        .route("/primary_lsa_summary/analyze", post(routes::primary_lsa_summary::analyze))
        .route("/primary_lsa_query_summary",         get(routes::primary_lsa_query_summary::page))
        .route("/primary_lsa_query_summary/results", get(routes::primary_lsa_query_summary::results))
        .route("/primary_lsa_query_summary/analyze", post(routes::primary_lsa_query_summary::analyze))
        .route("/anomaly_recent",         get(routes::anomaly_recent::page))
        .route("/anomaly_recent/results", get(routes::anomaly_recent::results))
        .route("/anomaly_recent/analyze", post(routes::anomaly_recent::analyze))
        .route("/denoise_recent",         get(routes::denoise_recent::page))
        .route("/denoise_recent/results", get(routes::denoise_recent::results))
        .route("/denoise_recent/analyze", post(routes::denoise_recent::analyze))
        .route("/knn",                    get(routes::knn::page))
        .route("/knn/results",            get(routes::knn::results))
        .route("/knn/analyze",            post(routes::knn::analyze))
        .route("/chat",           get(routes::chat::page))
        .route("/chat/query",     post(routes::chat::query))
        .route("/chat/new",       post(routes::chat::new_session))
        .route("/chat/reset",     post(routes::chat::reset))
        .route("/bund",           get(routes::bund::page))
        .route("/bund/eval",      post(routes::bund::eval))
        .route("/bund/translate", post(routes::bund::translate))
        .route("/scripts",                get(routes::scripts::page))
        .route("/scripts/list",           get(routes::scripts::list))
        .route("/scripts/editor",         get(routes::scripts::editor_new))
        .route("/scripts/editor/{id}",    get(routes::scripts::editor_get))
        .route("/scripts/save",           post(routes::scripts::save))
        .route("/scripts/run",            post(routes::scripts::run))
        .route("/scripts/{id}",           delete(routes::scripts::delete))
        .route("/version",        get(routes::version::version))
        .route("/whoami",         get(routes::whoami::whoami))
        .route("/healthz",        get(routes::healthz::healthz))

        // ── Authentication ──────────────────────────────────────────
        // GET stays here (unlimited); POST is split out onto a
        // rate-limited sub-router below and merged in.
        .route("/login",  get(routes::login::page))
        .route("/logout", post(routes::login::logout))

        // ── Administration → User management ─────────────────────────
        .route("/admin/users",                       get(routes::admin_users::page_with_banners))
        .route("/admin/users/add",                   post(routes::admin_users::add))
        .route("/admin/users/delete/{id}",           post(routes::admin_users::delete))
        .route("/admin/users/reset_password/{id}",   post(routes::admin_users::reset_password))
        .route("/admin/users/disable/{id}", post(routes::admin_users::disable))
        .route("/admin/users/enable/{id}",  post(routes::admin_users::enable))

        // ── Administration → LLM ─────────────────────────────────────
        .route("/admin/llm",       get(routes::admin_llm::page_with_banners))
        .route("/admin/llm/purge", post(routes::admin_llm::purge))
        .route("/help",            get(routes::help::page))
        .route("/help/query",      post(routes::help::query))

        // Gate every other route behind the session-cookie middleware.
        // Open paths (/login, /logout, /version) are checked inside the
        // middleware so we don't have to remove their layers here.
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_session))
        .layer(CompressionLayer::new());

    // POST /login lives on its own sub-router with a tower_governor
    // rate-limit layer.  Configured per-IP via the cluster config's
    // auth_rate_limit_per_minute knob; merged into the main app so
    // the URL stays /login.  The per-username limiter in
    // bdsnode/v3/user.authenticate runs on top of this — two
    // independent defences against brute force.
    let app = if cfg.auth_rate_limit_per_minute == 0 {
        log::info!("bdsweb /login rate limiting disabled (auth_rate_limit_per_minute=0)");
        app.route("/login", post(routes::login::submit))
    } else {
        let per_request_ms = (60_000_f64 / cfg.auth_rate_limit_per_minute as f64) as u64;
        log::info!(
            "bdsweb /login rate limit: {} requests/min/IP (burst={}, key={})",
            cfg.auth_rate_limit_per_minute, cfg.auth_rate_limit_per_minute,
            if cfg.trusted_proxy { "X-Forwarded-For (web.trusted_proxy)" } else { "peer IP" },
        );
        // M4: behind a reverse proxy the connection peer IP is the
        // proxy's — every client would share one bucket.  When
        // `web.trusted_proxy` is set, key on the forwarded client IP
        // instead.  Only honour it when explicitly configured: an
        // untrusted client can spoof `X-Forwarded-For` to dodge the
        // limit.  Each branch builds a differently-typed config, so
        // the limited sub-router is constructed inside the branch.
        let limited = if cfg.trusted_proxy {
            let mut builder = GovernorConfigBuilder::default().key_extractor(SmartIpKeyExtractor);
            builder
                .per_millisecond(per_request_ms.max(1))
                .burst_size(cfg.auth_rate_limit_per_minute);
            let governor_cfg = builder.finish().expect("governor config valid");
            Router::<AppState>::new()
                .route("/login", post(routes::login::submit))
                .layer(GovernorLayer { config: Arc::new(governor_cfg) })
        } else {
            let governor_cfg = GovernorConfigBuilder::default()
                .per_millisecond(per_request_ms.max(1))
                .burst_size(cfg.auth_rate_limit_per_minute)
                .finish()
                .expect("governor config valid");
            Router::<AppState>::new()
                .route("/login", post(routes::login::submit))
                .layer(GovernorLayer { config: Arc::new(governor_cfg) })
        };
        app.merge(limited)
    };

    // CSRF backstop + security headers wrap the fully-composed router
    // so they also cover /login, /logout, and the rate-limited
    // sub-router.  `CatchPanicLayer` turns a panic in any handler OR
    // inner middleware into a clean 500 (H2).  `htmx_error_fragment`
    // is added last → outermost, so it sees the final 500 (whether
    // from `AppError` or a caught panic) and, for HTMX requests,
    // swaps the full-page error doc for a compact fragment (H3).
    let app = app
        .layer(middleware::from_fn(security::require_same_origin))
        .layer(middleware::from_fn(security::set_security_headers))
        .layer(CatchPanicLayer::new())
        .layer(middleware::from_fn(error::htmx_error_fragment));

    // Bind state on the composed Router.  This converts
    // `Router<AppState>` → `Router<()>` so axum::serve can use
    // `into_make_service_with_connect_info`.
    let app = app.with_state(state);

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));

    log::info!("bdsweb listening on http://{addr}  →  bdsnode at {}", args.node);
    // ConnectInfo enables tower_governor's PeerIpKeyExtractor on the
    // /login POST sub-router — without it the per-IP rate limiter
    // has nothing to key on and rejects every request.
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("server error");
}
