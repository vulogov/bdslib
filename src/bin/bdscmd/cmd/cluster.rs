//! `bdscmd cluster <subcommand>` — drive cluster RPCs.
//!
//! Subcommands fall into two groups:
//!
//! 1. **Membership** (`status`, `peers`) — call HMAC-authenticated
//!    `v3/cluster.*` methods.  Require `--secret` (or `BDSCMD_CLUSTER_SECRET`)
//!    matching the target node's `cluster.shared_secret`.
//!
//! 2. **Distributed reads** (`timeline`, `count`, `search`, `knn`, `anomaly`,
//!    `denoise`) — call unauthenticated `v3/*` methods.  No secret required;
//!    the trust boundary is the same as for v2/* (you can already hit the
//!    bdsnode HTTP endpoint).

use anyhow::{bail, Context, Result};
use bdslib::cluster::hmac_auth;
use clap::{Args, Subcommand};
use serde_json::{json, Map, Value};

#[derive(Args)]
pub struct Cmd {
    /// Shared cluster secret for membership subcommands (`status`, `peers`).
    /// Ignored by distributed-read subcommands.
    #[arg(short, long, env = "BDSCMD_CLUSTER_SECRET", default_value = "")]
    secret: String,

    #[command(subcommand)]
    sub: Sub,
}

#[derive(Subcommand)]
enum Sub {
    /// (membership) Cluster mode, peer counts, replication factor — `v3/cluster.status`
    Status,
    /// (membership) Full peer table snapshot — `v3/cluster.peers`
    Peers,

    /// (distributed read) `v3/timeline` — earliest+latest timestamps across the cluster
    Timeline,
    /// (distributed read) `v3/count` — total record count across the cluster
    Count(CountArgs),
    /// (distributed read) `v3/search` — semantic vector search across the cluster, dedup by UUID
    Search(SearchArgs),

    /// (distributed analytics) `v3/knn` — k-NN over union of all peers' fingerprints
    Knn(KnnArgs),
    /// (distributed analytics) `v3/anomaly.recent` — n-gram phrase-rarity outliers
    Anomaly(AnomalyArgs),
    /// (distributed analytics) `v3/denoise.recent` — n-gram noise removal
    Denoise(DenoiseArgs),

    /// (replicated write) `v3/add` — write locally + fan-out to N-1 peers (fire-and-forget + hints)
    Add(AddArgs),
    /// (replicated write) `v3/add.batch` — same as `add` over an NDJSON file
    AddBatch(AddBatchArgs),

    /// (fully-replicated) `v3/doc.add` — add a doc, fan-out to ALL peers
    DocAdd(DocAddArgs),
    /// (fully-replicated) `v3/doc.delete` — delete a doc, tombstone, fan-out
    DocDelete(IdArgs),
    /// (fully-replicated) `v3/signal.emit` — emit a signal, fan-out to ALL peers
    SignalEmit(SignalEmitArgs),
    /// (fully-replicated) `v3/script.add` — add a BUND script, fan-out to ALL peers
    ScriptAdd(ScriptAddArgs),
    /// (fully-replicated) `v3/script.delete` — delete a script, tombstone, fan-out
    ScriptDelete(IdArgs),

    /// (admin) `v3/cluster.sync` — force an immediate hint replay + AE tick (HMAC required)
    Sync,
    /// (read) Per-peer hint backlog from `v2/cluster.peers` — no secret needed
    Hints,
}

#[derive(Args)]
pub struct CountArgs {
    /// Optional lookback window (e.g. `"1h"`).  Omit for all-time.
    #[arg(short, long)]
    duration: Option<String>,
}

#[derive(Args)]
pub struct SearchArgs {
    #[arg(short, long)] query:    String,
    #[arg(short, long)] duration: String,
    #[arg(short, long, default_value_t = 10)] limit: usize,
}

#[derive(Args)]
pub struct KnnArgs {
    #[arg(short, long)] duration: String,
    #[arg(short, long, default_value_t = 5)] k: usize,
    #[arg(long, default_value_t = 2)] min_word_len: usize,
    #[arg(long, default_value_t = 0.2)] anomaly_threshold: f32,
    #[arg(long, default_value_t = 10)] max_cluster_members: usize,
    #[arg(long, default_value_t = 20)] max_anomalies: usize,
}

#[derive(Args)]
pub struct AnomalyArgs {
    #[arg(short, long)] duration: String,
    #[arg(short, long, default_value_t = 2)] n: usize,
    #[arg(long, default_value_t = 2)] min_word_len: usize,
    #[arg(long, default_value_t = 0.7)] anomaly_threshold: f32,
    #[arg(long, default_value_t = 20)] max_anomalies: usize,
    #[arg(long, default_value_t = 5)] max_novel_ngrams: usize,
}

#[derive(Args)]
pub struct AddArgs {
    /// Telemetry document as a JSON string (must contain `timestamp`, `key`, `data`).
    #[arg(short = 'D', long)]
    doc: String,
    /// Override the cluster's configured replication factor.
    #[arg(short = 'r', long)]
    replication_factor: Option<usize>,
}

#[derive(Args)]
pub struct AddBatchArgs {
    /// Newline-delimited JSON file (one document per line).
    #[arg(short = 'f', long)]
    file: String,
    /// Override the cluster's configured replication factor.
    #[arg(short = 'r', long)]
    replication_factor: Option<usize>,
}

#[derive(Args)]
pub struct DocAddArgs {
    /// Document metadata as a JSON string.
    #[arg(short = 'm', long)]
    metadata: String,
    /// Document content (UTF-8 string).
    #[arg(short = 'c', long)]
    content: String,
}

#[derive(Args)]
pub struct SignalEmitArgs {
    #[arg(short = 'n', long)]
    name: String,
    #[arg(short = 'S', long, default_value = "info")]
    severity: String,
    /// Unix-second timestamp.  Defaults to now.
    #[arg(short = 't', long)]
    timestamp: Option<u64>,
    /// Optional metadata as a JSON object string.
    #[arg(short = 'm', long)]
    metadata: Option<String>,
}

#[derive(Args)]
pub struct ScriptAddArgs {
    /// Script metadata as a JSON string (must contain `name` and `schedule`).
    #[arg(short = 'm', long)]
    metadata: String,
    /// BUND script body (UTF-8).
    #[arg(short = 'b', long)]
    body: String,
}

#[derive(Args)]
pub struct IdArgs {
    /// UUIDv7 of the document/script to delete.
    #[arg(short = 'i', long)]
    id: String,
}

#[derive(Args)]
pub struct DenoiseArgs {
    #[arg(short, long)] duration: String,
    #[arg(short, long, default_value_t = 2)] n: usize,
    #[arg(long, default_value_t = 2)] min_word_len: usize,
    #[arg(long, default_value_t = 0.85)] noise_threshold: f32,
    #[arg(long, default_value_t = 100)] max_kept: usize,
    #[arg(long, default_value_t = 100)] max_removed: usize,
}

fn signed_call(url: &str, method: &str, secret: &str, mut params: Map<String, Value>) -> Result<Value> {
    let canonical = serde_json::to_vec(&Value::Object(params.clone()))
        .context("serialize params for HMAC")?;
    let sig = hmac_auth::sign(secret, &canonical);
    params.insert("_hmac".into(), Value::String(sig));
    crate::client::call(url, method, Value::Object(params))
}

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    match args.sub {
        // Membership — require secret.
        Sub::Status | Sub::Peers | Sub::Sync => {
            if args.secret.is_empty() {
                bail!("--secret is required for membership/admin subcommands (or set BDSCMD_CLUSTER_SECRET)");
            }
            let method = match args.sub {
                Sub::Status => "v3/cluster.status",
                Sub::Peers  => "v3/cluster.peers",
                Sub::Sync   => "v3/cluster.sync",
                _           => unreachable!(),
            };
            signed_call(url, method, &args.secret, Map::new())
        }
        Sub::Hints => crate::client::call(url, "v2/cluster.peers", json!({})),

        // Distributed reads — no secret needed; v3/* methods are
        // client-to-coordinator (same trust boundary as v2/*).
        Sub::Timeline => crate::client::call(url, "v3/timeline", json!({})),
        Sub::Count(a) => {
            let mut p = json!({});
            if let Some(d) = &a.duration {
                p["duration"] = json!(d);
            }
            crate::client::call(url, "v3/count", p)
        }
        Sub::Search(a) => crate::client::call(url, "v3/search", json!({
            "query":    a.query,
            "duration": a.duration,
            "limit":    a.limit,
        })),
        Sub::Knn(a) => crate::client::call(url, "v3/knn", json!({
            "duration":            a.duration,
            "k":                   a.k,
            "min_word_len":        a.min_word_len,
            "anomaly_threshold":   a.anomaly_threshold,
            "max_cluster_members": a.max_cluster_members,
            "max_anomalies":       a.max_anomalies,
        })),
        Sub::Anomaly(a) => crate::client::call(url, "v3/anomaly.recent", json!({
            "duration":          a.duration,
            "n":                 a.n,
            "min_word_len":      a.min_word_len,
            "anomaly_threshold": a.anomaly_threshold,
            "max_anomalies":     a.max_anomalies,
            "max_novel_ngrams":  a.max_novel_ngrams,
        })),
        Sub::Denoise(a) => crate::client::call(url, "v3/denoise.recent", json!({
            "duration":        a.duration,
            "n":               a.n,
            "min_word_len":    a.min_word_len,
            "noise_threshold": a.noise_threshold,
            "max_kept":        a.max_kept,
            "max_removed":     a.max_removed,
        })),
        Sub::Add(a) => {
            let doc: Value = serde_json::from_str(&a.doc)
                .with_context(|| format!("--doc is not valid JSON: {:?}", a.doc))?;
            let mut params = json!({ "doc": doc });
            if let Some(rf) = a.replication_factor {
                params["replication_factor"] = json!(rf);
            }
            crate::client::call(url, "v3/add", params)
        }
        Sub::AddBatch(a) => {
            let raw = std::fs::read_to_string(&a.file)
                .with_context(|| format!("read {}", a.file))?;
            let mut docs: Vec<Value> = Vec::new();
            for (i, line) in raw.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.is_empty() { continue; }
                docs.push(serde_json::from_str(trimmed)
                    .with_context(|| format!("{}:{}: invalid JSON", a.file, i + 1))?);
            }
            let mut params = json!({ "docs": docs });
            if let Some(rf) = a.replication_factor {
                params["replication_factor"] = json!(rf);
            }
            crate::client::call(url, "v3/add.batch", params)
        }
        Sub::DocAdd(a) => {
            let metadata: Value = serde_json::from_str(&a.metadata)
                .with_context(|| format!("--metadata is not valid JSON: {:?}", a.metadata))?;
            crate::client::call(url, "v3/doc.add", json!({
                "metadata": metadata,
                "content":  a.content,
            }))
        }
        Sub::DocDelete(a) => crate::client::call(url, "v3/doc.delete", json!({ "id": a.id })),
        Sub::SignalEmit(a) => {
            let metadata: Value = match &a.metadata {
                Some(s) => serde_json::from_str(s)
                    .with_context(|| format!("--metadata is not valid JSON: {s:?}"))?,
                None => json!({}),
            };
            let timestamp = a.timestamp.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            });
            crate::client::call(url, "v3/signal.emit", json!({
                "name":      a.name,
                "severity":  a.severity,
                "timestamp": timestamp,
                "metadata":  metadata,
            }))
        }
        Sub::ScriptAdd(a) => {
            let metadata: Value = serde_json::from_str(&a.metadata)
                .with_context(|| format!("--metadata is not valid JSON: {:?}", a.metadata))?;
            crate::client::call(url, "v3/script.add", json!({
                "metadata": metadata,
                "script":   a.body,
            }))
        }
        Sub::ScriptDelete(a) => crate::client::call(url, "v3/script.delete", json!({ "id": a.id })),
    }
}
