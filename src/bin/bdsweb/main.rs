mod client;
mod error;
mod routes;
mod state;

use axum::{routing::{get, post}, Router};
use clap::Parser;
use state::AppState;
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

fn ollama_model_from_config(config_path: Option<&str>) -> String {
    let path = match config_path {
        Some(p) => p,
        None => return "llama3.2".to_owned(),
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return "llama3.2".to_owned(),
    };
    let val: serde_hjson::Value = match serde_hjson::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return "llama3.2".to_owned(),
    };
    val.as_object()
       .and_then(|o| o.get("ollama_model"))
       .and_then(|v| v.as_str())
       .unwrap_or("llama3.2")
       .to_owned()
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

    let ollama_model = ollama_model_from_config(args.config.as_deref());
    let state = AppState::new(args.node.clone(), ollama_model);

    let app = Router::new()
        .route("/",               get(routes::dashboard::handler))
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
        .route("/trends",         get(routes::trends::page))
        .route("/trends/results", get(routes::trends::results))
        .route("/rca",            get(routes::rca::page))
        .route("/rca/results",    get(routes::rca::results))
        .route("/chat",           get(routes::chat::page))
        .route("/chat/query",     post(routes::chat::query))
        .route("/chat/new",       post(routes::chat::new_session))
        .route("/chat/reset",     get(routes::chat::reset))
        .route("/bund",           get(routes::bund::page))
        .route("/bund/eval",      post(routes::bund::eval))
        .layer(CompressionLayer::new())
        .with_state(state);

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));

    log::info!("bdsweb listening on http://{addr}  →  bdsnode at {}", args.node);
    axum::serve(listener, app).await.expect("server error");
}
