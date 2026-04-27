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

    /// Log verbosity (0=warn, 1=info, 2=debug)
    #[arg(long, default_value_t = 1)]
    verbose: u8,
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

    let state = AppState::new(args.node.clone());

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
