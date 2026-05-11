mod admin;
mod auth;
mod client;
mod error;
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

    /// Path to bds.hjson config file (reads ollama_model for the Chat UI)
    #[arg(short, long, env = "BDS_CONFIG")]
    config: Option<String>,

    /// Log verbosity (0=warn, 1=info, 2=debug)
    #[arg(long, default_value_t = 1)]
    verbose: u8,
}

struct WebConfig {
    ollama_model:           String,
    dashboard_refresh_secs: u64,
    /// Cluster shared secret read from `cluster.shared_secret` in
    /// `bds.hjson`.  Empty when the config file is missing, the
    /// cluster block is absent, or `cluster.enabled = false` — in
    /// all three cases bdsweb runs in open-access mode (no auth
    /// middleware, no login form).
    shared_secret:          String,
    /// `cluster.auth_rate_limit_per_minute` — Phase 6 rate limit
    /// applied per-IP to `POST /login`.  `0` disables the limit.
    auth_rate_limit_per_minute: u32,
}

fn load_config(config_path: Option<&str>) -> WebConfig {
    let defaults = WebConfig {
        ollama_model: "llama3.2".to_owned(),
        dashboard_refresh_secs: 30,
        shared_secret: String::new(),
        auth_rate_limit_per_minute: 10,
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
    WebConfig {
        ollama_model: obj.get("ollama_model")
            .and_then(|v| v.as_str())
            .unwrap_or(&defaults.ollama_model)
            .to_owned(),
        dashboard_refresh_secs: obj.get("dashboard_refresh_secs")
            .and_then(|v| v.as_f64())
            .map(|n| n as u64)
            .unwrap_or(defaults.dashboard_refresh_secs)
            .max(1),
        shared_secret,
        auth_rate_limit_per_minute,
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
    let state = AppState::new(args.node.clone(), cfg.ollama_model, cfg.dashboard_refresh_secs,
                              cfg.shared_secret);

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

    let app = Router::new()
        .route("/",                  get(routes::dashboard::page))
        .route("/dashboard/data",    get(routes::dashboard::data))
        .route("/dashboard/refresh", get(routes::dashboard::refresh))
        .route("/cluster",           get(routes::cluster::page))
        .route("/telemetry",         get(routes::telemetry::page))
        .route("/telemetry/results", get(routes::telemetry::results))
        .route("/telemetry/keys",    get(routes::telemetry::keys))
        .route("/logs",              get(routes::logs::page))
        .route("/logs/results",      get(routes::logs::results))
        .route("/logs/keys",         get(routes::logs::keys))
        .route("/logs/topics",       get(routes::logs::topics))
        .route("/docs",           get(routes::docs::page))
        .route("/docs/results",   get(routes::docs::results))
        .route("/search",         get(routes::search::page))
        .route("/search/results", get(routes::search::results))
        .route("/trends",           get(routes::trends::page))
        .route("/trends/results",   get(routes::trends::results))
        .route("/signals",          get(routes::signals::page))
        .route("/signals/results",  get(routes::signals::results))
        .route("/rca",              get(routes::rca::page))
        .route("/rca/results",    get(routes::rca::results))
        .route("/rca/templates",         get(routes::rca_templates::page))
        .route("/rca/templates/results", get(routes::rca_templates::results))
        .route("/templates",         get(routes::templates::page))
        .route("/templates/results", get(routes::templates::results))
        .route("/templates_summary",         get(routes::templates_summary::page))
        .route("/templates_summary/results", get(routes::templates_summary::results))
        .route("/primary_summary",         get(routes::primary_summary::page))
        .route("/primary_summary/results", get(routes::primary_summary::results))
        .route("/primary_query_summary",         get(routes::primary_query_summary::page))
        .route("/primary_query_summary/results", get(routes::primary_query_summary::results))
        .route("/primary_lsa_summary",         get(routes::primary_lsa_summary::page))
        .route("/primary_lsa_summary/results", get(routes::primary_lsa_summary::results))
        .route("/primary_lsa_query_summary",         get(routes::primary_lsa_query_summary::page))
        .route("/primary_lsa_query_summary/results", get(routes::primary_lsa_query_summary::results))
        .route("/anomaly_recent",         get(routes::anomaly_recent::page))
        .route("/anomaly_recent/results", get(routes::anomaly_recent::results))
        .route("/denoise_recent",         get(routes::denoise_recent::page))
        .route("/denoise_recent/results", get(routes::denoise_recent::results))
        .route("/knn",                    get(routes::knn::page))
        .route("/knn/results",            get(routes::knn::results))
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
