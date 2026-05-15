use anyhow::Result;
use clap::Args;
use serde_json::Value;

#[derive(Args)]
pub struct Cmd {
    /// Absolute path to the RFC 3164 syslog file on the server's filesystem
    path: String,

    /// Explicit source override applied to every syslog record
    /// parsed from the file.  When omitted, per-record resolution
    /// promotes the parsed RFC 3164 `host` field to `source` —
    /// natural choice when the operator wants per-host grouping.
    /// Pass an explicit value (e.g. `pipeline-a`) to override the
    /// host and assign every record from this file to one logical
    /// pipeline source.
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
    crate::client::call(url, "v2/add.file.syslog", Value::Object(params))
}
