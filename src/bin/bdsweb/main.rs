mod admin;
mod auth;
mod client;
mod error;
mod error_pretty;
mod markdown;
mod routes;
mod state;

use axum::{middleware, routing::{delete, get, post}, Router};
use clap::Parser;
use state::AppState;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};
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
}

fn load_config(config_path: Option<&str>) -> WebConfig {
    let defaults = WebConfig {
        dashboard_refresh_secs: 30,
        cluster_refresh_secs:   10,
        shared_secret: String::new(),
        auth_rate_limit_per_minute: 10,
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
    if cfg.shared_secret.is_empty() {
        log::warn!("bdsweb starting in OPEN-ACCESS mode — no cluster.shared_secret found in config; \
                    session auth is disabled");
    } else {
        log::info!("bdsweb auth enabled — shared_secret loaded ({} bytes)", cfg.shared_secret.len());
    }
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
    let state = AppState::new(
        args.node.clone(),
        cfg.dashboard_refresh_secs,
        cfg.cluster_refresh_secs,
        cfg.shared_secret,
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
    );

    // Background poller: refreshes the cached Dashboard snapshot every N seconds.
    {
        let poller_state = state.clone();
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(poller_state.dashboard_refresh_secs);
            log::info!(
                "dashboard background poller started (interval={}s)",
                poller_state.dashboard_refresh_secs
            );
            loop {
                match routes::dashboard::collect(&poller_state).await {
                    Ok(snap) => {
                        *poller_state.dashboard_cache.write().await = Some(snap);
                        log::debug!("dashboard cache refreshed");
                    }
                    Err(e) => {
                        log::warn!("dashboard background poll failed: {e}");
                    }
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    // Background poller: same recipe as the dashboard one but for the
    // Cluster page.  Polls v2/cluster.peers and parks the response in
    // `state.cluster_cache` so /cluster/data renders without hitting
    // bdsnode on every page load.
    {
        let poller_state = state.clone();
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(poller_state.cluster_refresh_secs);
            log::info!(
                "cluster background poller started (interval={}s)",
                poller_state.cluster_refresh_secs
            );
            loop {
                match routes::cluster::collect(&poller_state).await {
                    Ok(snap) => {
                        *poller_state.cluster_cache.write().await = Some(snap);
                        log::debug!("cluster cache refreshed");
                    }
                    Err(e) => {
                        log::warn!("cluster background poll failed: {e}");
                    }
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    let app = Router::new()
        .route("/",                  get(routes::dashboard::page))
        .route("/dashboard/data",    get(routes::dashboard::data))
        .route("/dashboard/refresh", get(routes::dashboard::refresh))
        .route("/cluster",           get(routes::cluster::page))
        .route("/cluster/data",      get(routes::cluster::data))
        .route("/cluster/refresh",   get(routes::cluster::refresh))
        .route("/telemetry",         get(routes::telemetry::page))
        .route("/telemetry/results", get(routes::telemetry::results))
        .route("/telemetry/keys",    get(routes::telemetry::keys))
        .route("/telemetry/analyze", get(routes::telemetry::analyze))
        .route("/logs",              get(routes::logs::page))
        .route("/logs/results",      get(routes::logs::results))
        .route("/logs/keys",         get(routes::logs::keys))
        .route("/logs/topics",       get(routes::logs::topics))
        .route("/logs/analyze",      get(routes::logs::analyze))
        .route("/docs",           get(routes::docs::page))
        .route("/docs/results",   get(routes::docs::results))
        .route("/search",         get(routes::search::page))
        .route("/search/results", get(routes::search::results))
        .route("/search/analyze", get(routes::search::analyze))
        .route("/trends",           get(routes::trends::page))
        .route("/trends/results",   get(routes::trends::results))
        .route("/signals",          get(routes::signals::page))
        .route("/signals/results",  get(routes::signals::results))
        .route("/rca",              get(routes::rca::page))
        .route("/rca/results",    get(routes::rca::results))
        .route("/rca/analyze",    get(routes::rca::analyze))
        .route("/rca/templates",         get(routes::rca_templates::page))
        .route("/rca/templates/results", get(routes::rca_templates::results))
        .route("/rca/templates/analyze", get(routes::rca_templates::analyze))
        .route("/templates",         get(routes::templates::page))
        .route("/templates/results", get(routes::templates::results))
        .route("/templates/analyze", get(routes::templates::analyze))
        .route("/templates_summary",         get(routes::templates_summary::page))
        .route("/templates_summary/results", get(routes::templates_summary::results))
        .route("/templates_summary/analyze", get(routes::templates_summary::analyze))
        .route("/primary_summary",         get(routes::primary_summary::page))
        .route("/primary_summary/results", get(routes::primary_summary::results))
        .route("/primary_summary/analyze", get(routes::primary_summary::analyze))
        .route("/primary_query_summary",         get(routes::primary_query_summary::page))
        .route("/primary_query_summary/results", get(routes::primary_query_summary::results))
        .route("/primary_query_summary/analyze", get(routes::primary_query_summary::analyze))
        .route("/primary_lsa_summary",         get(routes::primary_lsa_summary::page))
        .route("/primary_lsa_summary/results", get(routes::primary_lsa_summary::results))
        .route("/primary_lsa_summary/analyze", get(routes::primary_lsa_summary::analyze))
        .route("/primary_lsa_query_summary",         get(routes::primary_lsa_query_summary::page))
        .route("/primary_lsa_query_summary/results", get(routes::primary_lsa_query_summary::results))
        .route("/primary_lsa_query_summary/analyze", get(routes::primary_lsa_query_summary::analyze))
        .route("/anomaly_recent",         get(routes::anomaly_recent::page))
        .route("/anomaly_recent/results", get(routes::anomaly_recent::results))
        .route("/anomaly_recent/analyze", get(routes::anomaly_recent::analyze))
        .route("/denoise_recent",         get(routes::denoise_recent::page))
        .route("/denoise_recent/results", get(routes::denoise_recent::results))
        .route("/denoise_recent/analyze", get(routes::denoise_recent::analyze))
        .route("/knn",                    get(routes::knn::page))
        .route("/knn/results",            get(routes::knn::results))
        .route("/knn/analyze",            get(routes::knn::analyze))
        .route("/chat",           get(routes::chat::page))
        .route("/chat/query",     post(routes::chat::query))
        .route("/chat/new",       post(routes::chat::new_session))
        .route("/chat/reset",     get(routes::chat::reset))
        .route("/bund",           get(routes::bund::page))
        .route("/bund/eval",      post(routes::bund::eval))
        .route("/scripts",                get(routes::scripts::page))
        .route("/scripts/list",           get(routes::scripts::list))
        .route("/scripts/editor",         get(routes::scripts::editor_new))
        .route("/scripts/editor/{id}",    get(routes::scripts::editor_get))
        .route("/scripts/save",           post(routes::scripts::save))
        .route("/scripts/run",            post(routes::scripts::run))
        .route("/scripts/{id}",           delete(routes::scripts::delete))
        .route("/version",        get(routes::version::version))
        .route("/whoami",         get(routes::whoami::whoami))

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
        let governor_cfg = GovernorConfigBuilder::default()
            .per_millisecond(per_request_ms.max(1))
            .burst_size(cfg.auth_rate_limit_per_minute)
            .finish()
            .expect("governor config valid");
        log::info!(
            "bdsweb /login rate limit: {} requests/min/IP (burst={})",
            cfg.auth_rate_limit_per_minute, cfg.auth_rate_limit_per_minute,
        );
        let limited = Router::<AppState>::new()
            .route("/login", post(routes::login::submit))
            .layer(GovernorLayer { config: Arc::new(governor_cfg) });
        app.merge(limited)
    };

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
