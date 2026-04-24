mod jsonrpc;
mod server;

use anyhow::Context;
use clap::Parser;
use jsonrpsee::server::Server;

#[derive(Parser)]
#[command(name = "bdsnode", about = "BDS JSON-RPC 2.0 server")]
struct Cli {
    /// Path to the hjson configuration file (overrides BDS_CONFIG env var).
    #[arg(short, long, env = "BDS_CONFIG")]
    config: Option<String>,

    /// Address to bind the JSON-RPC listener.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Port for the JSON-RPC listener.
    #[arg(short, long, default_value_t = 9000)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    bdslib::init_db(cli.config.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to initialise database")?;

    bdslib::init_adam()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to initialise BUND VM")?;

    bdslib::context::init(cli.config.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to initialise BUND context")?;

    bdslib::pipe::init(&["ingest"])
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to initialise pipe registry")?;

    let add_handle = if let Some(cfg) = server::add::Config::from_config(cli.config.as_deref())
        .context("failed to read ingest config")?
    {
        Some(server::add::start(cfg))
    } else {
        None
    };

    let addr = format!("{}:{}", cli.host, cli.port);

    let server = Server::builder()
        .build(&addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    let local_addr = server.local_addr()?;
    let handle = server.start(jsonrpc::build_module());

    eprintln!("bdsnode listening on {local_addr}");

    tokio::signal::ctrl_c().await.context("ctrl-c signal error")?;

    eprintln!("shutting down…");
    handle.stop()?;
    handle.stopped().await;

    // Drain the ingest channel and join the batch thread before checkpointing
    // so that no queued records are lost.
    if let Some(h) = add_handle {
        h.stop();
    }

    bdslib::sync_db().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
