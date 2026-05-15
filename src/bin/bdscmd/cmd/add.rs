use anyhow::{Context, Result};
use clap::Args;
use serde_json::Value;

#[derive(Args)]
pub struct Cmd {
    /// JSON document to ingest (reads from stdin if omitted)
    doc: Option<String>,

    /// Explicit source override (e.g. host name, pipeline label).
    /// Beats every other resolution step (top-level keys in the doc,
    /// `data.*` keys, deployment default).  When omitted, the doc
    /// falls through to the resolution chain — typically lands the
    /// `"global"` default unless the doc carries a `source`,
    /// `origin`, or `host` key (top-level or nested under `data`).
    #[arg(long)]
    source: Option<String>,

    /// When `true`, bypass the ingest queue and wait until the
    /// record is durably stored.  The response carries the
    /// assigned UUID instead of a queue ack.  Forwarded to
    /// `v2/add`'s `sync` param.
    #[arg(long)]
    sync: bool,
}

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    let raw = match args.doc {
        Some(s) => s,
        None => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .context("failed to read doc from stdin")?;
            buf
        }
    };
    let doc: Value = serde_json::from_str(raw.trim()).context("invalid JSON document")?;
    let mut params = serde_json::Map::new();
    params.insert("doc".into(), doc);
    if args.sync {
        params.insert("sync".into(), Value::Bool(true));
    }
    if let Some(s) = args.source {
        params.insert("source".into(), Value::String(s));
    }
    crate::client::call(url, "v2/add", Value::Object(params))
}
