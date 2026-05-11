use anyhow::{Context, Result};
use clap::Args;
use serde_json::Value;

#[derive(Args)]
pub struct Cmd {
    /// Script source: path to a .bund file, "-" or omitted for stdin.
    ///
    /// When used as a shebang interpreter (#!/path/to/bdscmd eval), the kernel
    /// passes the script file path here automatically.
    source: Option<String>,

    /// BUND VM context name
    #[arg(short, long, default_value = "default")]
    context: String,

    /// Print a one-line summary of the script's cluster_meta to stderr
    /// after the result.  Useful for verifying that `cls.*` calls
    /// actually replicated / fanned out instead of silently falling
    /// back to local-only mode.  Has no effect when the response
    /// carries no cluster_meta (standalone bdsnode, or no `cls.*` was
    /// called).
    #[arg(short = 'm', long)]
    cluster_meta: bool,
}

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    let raw = match args.source.as_deref() {
        None | Some("-") => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .context("failed to read script from stdin")?;
            buf
        }
        Some(path) => {
            std::fs::read_to_string(path).with_context(|| format!("cannot read script {path}"))?
        }
    };

    // Strip shebang line so scripts can begin with #!/path/to/bdscmd eval
    let script = if raw.starts_with("#!") {
        raw.splitn(2, '\n').nth(1).unwrap_or("").to_string()
    } else {
        raw
    };

    let result = crate::client::call(
        url,
        "v2/eval",
        serde_json::json!({ "context": args.context, "script": script }),
    )?;

    if args.cluster_meta {
        if let Some(meta) = result.get("cluster_meta") {
            if meta.is_object() {
                eprintln!("cluster_meta: {}", summarise_meta(meta));
            }
        }
    }

    Ok(result)
}

/// One-line, human-readable digest of the cluster_meta block — used by
/// `--cluster-meta` so operators can see "did this script actually go
/// through the cluster?" without having to grep the JSON.
fn summarise_meta(meta: &Value) -> String {
    let enabled = meta.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    if !enabled {
        return "standalone (no cluster fan-out)".to_owned();
    }
    if let Some(rep) = meta.get("replication").and_then(|v| v.as_object()) {
        let attempted = rep.get("peers_attempted").and_then(|v| v.as_u64()).unwrap_or(0);
        let succeeded = rep.get("peers_succeeded").and_then(|v| v.as_u64()).unwrap_or(0);
        let hinted    = rep.get("hints_queued").and_then(|v| v.as_u64()).unwrap_or(0);
        return format!("write replicated to {succeeded}/{attempted} peers ({hinted} hinted)");
    }
    let queried  = meta.get("peers_queried").and_then(|v| v.as_u64()).unwrap_or(0);
    let answered = meta.get("peers_answered").and_then(|v| v.as_u64()).unwrap_or(0);
    let partial  = meta.get("partial").and_then(|v| v.as_bool()).unwrap_or(false);
    let suffix   = if partial { " (PARTIAL)" } else { "" };
    format!("read fanned out to {queried} peer{}, {answered} answered{suffix}",
            if queried == 1 { "" } else { "s" })
}
