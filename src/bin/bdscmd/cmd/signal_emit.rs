use anyhow::Result;
use clap::Args;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Args)]
pub struct Cmd {
    /// Signal name / category
    #[arg(short, long)]
    name: String,

    /// Signal severity (e.g. info, warning, critical)
    #[arg(short, long)]
    severity: String,

    /// Unix-second timestamp (defaults to now)
    #[arg(short, long)]
    timestamp: Option<u64>,
}

pub fn run(url: &str, session: &str, args: Cmd) -> Result<Value> {
    let ts = args.timestamp.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });
    crate::client::call(
        url,
        "v2/signal.emit",
        serde_json::json!({
            "session":   session,
            "name":      args.name,
            "severity":  args.severity,
            "timestamp": ts,
        }),
    )
}
