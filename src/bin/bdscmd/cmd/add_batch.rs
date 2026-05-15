use anyhow::{Context, Result};
use clap::Args;
use serde_json::Value;

#[derive(Args)]
pub struct Cmd {
    /// JSON array of documents, or NDJSON file path.  `-` or omitted
    /// reads from stdin.  (Renamed from `source` to `input` so the
    /// new `--source` flag doesn't collide with the positional.)
    input: Option<String>,

    /// Explicit source override applied to EVERY doc in the batch.
    /// Beats per-doc resolution from top-level / `data.*` keys.
    #[arg(long)]
    source: Option<String>,

    /// When `true`, bypass the ingest queue and wait until every
    /// record is durably stored.  Forwarded to `v2/add.batch`'s
    /// `sync` param.
    #[arg(long)]
    sync: bool,
}

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    let raw = match args.input {
        Some(ref path) if path != "-" => {
            std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?
        }
        _ => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .context("failed to read from stdin")?;
            buf
        }
    };

    let docs: Vec<Value> = if raw.trim_start().starts_with('[') {
        serde_json::from_str(raw.trim()).context("invalid JSON array")?
    } else {
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .map(|(i, l)| {
                serde_json::from_str(l).with_context(|| format!("invalid JSON on line {}", i + 1))
            })
            .collect::<Result<Vec<_>>>()?
    };

    let mut params = serde_json::Map::new();
    params.insert("docs".into(), Value::Array(docs));
    if args.sync {
        params.insert("sync".into(), Value::Bool(true));
    }
    if let Some(s) = args.source {
        params.insert("source".into(), Value::String(s));
    }
    crate::client::call(url, "v2/add.batch", Value::Object(params))
}
