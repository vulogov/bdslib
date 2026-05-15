use anyhow::Result;
use clap::Args;
use serde_json::Value;

#[derive(Args)]
pub struct Cmd {
    /// Absolute path to the NDJSON file on the server's filesystem
    path: String,

    /// Explicit source override applied to every record parsed from
    /// the file.  When omitted, per-record resolution runs (typically
    /// lands the deployment default since NDJSON records rarely
    /// carry a host/origin tag of their own).
    #[arg(long)]
    source: Option<String>,
}

pub fn run(url: &str, session: &str, args: Cmd) -> Result<Value> {
    let mut params = serde_json::Map::new();
    params.insert("session".into(), Value::String(session.to_owned()));
    params.insert("path".into(), Value::String(args.path));
    if let Some(s) = args.source {
        params.insert("source".into(), Value::String(s));
    }
    crate::client::call(url, "v2/add.file", Value::Object(params))
}
