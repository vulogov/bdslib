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

    // ── Phase 6: replicated file ingest ──────────────────────────────────
    /// (replicated write) `v3/add.file` — read NDJSON file + replicate via v3/add.batch
    AddFile(PathArgs),
    /// (replicated write) `v3/add.file.syslog` — read RFC 3164 syslog file + replicate
    AddFileSyslog(PathArgs),

    // ── Phase 6: cluster-wide reads ──────────────────────────────────────
    /// `v3/fulltext` — BM25 full-text search (ids+scores, UUID dedup + score average)
    Fulltext(QueryWithLimitArgs),
    /// `v3/fulltext.get` — BM25 full-text search returning full documents
    FulltextGet(QueryArgs),
    /// `v3/fulltext.recent` — BM25 full-text search, newest-first
    FulltextRecent(QueryWithLimitArgs),

    /// `v3/keys` — distinct primary-record keys in a window (sorted union)
    Keys(DurationArgs),
    /// `v3/keys.all` — keys matching a shell-glob pattern (sorted union)
    KeysAll(KeysAllArgs),
    /// `v3/keys.get` — primaries+secondaries for keys matching a pattern
    KeysGet(KeyAndDurationArgs),

    /// `v3/primaries` — UUID set union for the window
    Primaries(WindowArgs),
    /// `v3/primaries.explore` — keys with > 1 primary in the window
    PrimariesExplore(DurationArgs),
    /// `v3/primaries.explore.telemetry` — same, but only telemetry keys
    PrimariesExploreTelemetry(DurationArgs),
    /// `v3/primaries.get` — primary records for an exact key
    PrimariesGet(KeyAndDurationArgs),
    /// `v3/primaries.get.telemetry` — extracted numeric values for an exact key
    PrimariesGetTelemetry(KeyAndDurationArgs),

    /// `v3/topics` — LDA topic analysis for a single key (largest-corpus pick)
    Topics(TopicsArgs),
    /// `v3/topics.all` — LDA per distinct key (per-key largest-corpus pick)
    TopicsAll(TopicsAllArgs),

    /// `v3/signals` — recent signals (UUID dedup)
    Signals(DurationArgs),
    /// `v3/signals_query` — semantic search over signals (UUID dedup + score average)
    SignalsQuery(SignalsQueryArgs),

    /// `v3/search.get` — semantic vector search returning full docs (UUID dedup + score average)
    SearchGet(QueryWithLimitArgs),

    /// `v3/secondaries` — secondary UUIDs for a primary (cluster-wide UUID set union)
    Secondaries(PrimaryIdArgs),
    /// `v3/secondary` — fetch a secondary record by UUID (first-non-null-peer-wins)
    Secondary(SecondaryIdArgs),

    /// `v3/tpl.list` — list templates in a window (UUID dedup)
    TplList(DurationArgs),
    /// `v3/tpl.search` — semantic search over templates (UUID dedup + score average)
    TplSearch(QueryWithLimitArgs),
    /// `v3/tpl.get` — fetch a single template by UUID (first-non-null-peer-wins)
    TplGet(IdArgs),
    /// `v3/tpl.template_by_id` — fetch via FrequencyTracking (cross-shard)
    TplTemplateById(IdArgs),
    /// `v3/tpl.templates_recent` — templates observed in a window (UUID dedup)
    TplTemplatesRecent(DurationArgs),
    /// `v3/tpl.templates_by_timestamp` — templates observed in a Unix-second range
    TplTemplatesByTimestamp(TsRangeArgs),
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

// ── Phase 6 args ─────────────────────────────────────────────────────────────

#[derive(Args)]
pub struct PathArgs {
    /// File path on the receiving bdsnode's filesystem.
    #[arg(short = 'p', long)]
    path: String,
    #[arg(short = 'r', long)]
    replication_factor: Option<usize>,
}

#[derive(Args)]
pub struct DurationArgs {
    /// Lookback window (humantime, e.g. "1h", "30min").
    #[arg(short, long)]
    duration: String,
}

#[derive(Args)]
pub struct WindowArgs {
    /// Optional lookback window.  Omit for all-time.
    #[arg(short, long)]
    duration: Option<String>,
    /// Optional explicit range start (Unix seconds).  Requires --end-ts.
    #[arg(long)]
    start_ts: Option<i64>,
    /// Optional explicit range end (Unix seconds).
    #[arg(long)]
    end_ts: Option<i64>,
}

#[derive(Args)]
pub struct KeyAndDurationArgs {
    #[arg(short, long)] duration: String,
    #[arg(short, long)] key: String,
}

#[derive(Args)]
pub struct KeysAllArgs {
    #[arg(short, long)] duration: String,
    /// Shell-glob pattern (default `*` for "all keys").
    #[arg(short, long, default_value = "*")]
    key: String,
}

#[derive(Args)]
pub struct QueryArgs {
    #[arg(short, long)] query:    String,
    #[arg(short, long)] duration: String,
}

#[derive(Args)]
pub struct QueryWithLimitArgs {
    #[arg(short, long)] query:    String,
    #[arg(short, long)] duration: String,
    #[arg(short, long, default_value_t = 10)] limit: usize,
}

#[derive(Args)]
pub struct SignalsQueryArgs {
    #[arg(short, long)] query: String,
    #[arg(short, long, default_value_t = 20)] limit: usize,
}

#[derive(Args)]
pub struct TopicsArgs {
    #[arg(short, long)] key:      String,
    #[arg(short, long)] duration: String,
    #[arg(short, long, default_value_t = 3)]   k:     usize,
    #[arg(long, default_value_t = 0.1)]   alpha: f64,
    #[arg(long, default_value_t = 0.01)]  beta:  f64,
    #[arg(long, default_value_t = 42)]    seed:  u64,
    #[arg(long, default_value_t = 200)]   iters: usize,
    #[arg(long, default_value_t = 10)]    top_n: usize,
}

#[derive(Args)]
pub struct TopicsAllArgs {
    #[arg(short, long)] duration: String,
    #[arg(short, long, default_value_t = 3)]   k:     usize,
    #[arg(long, default_value_t = 0.1)]   alpha: f64,
    #[arg(long, default_value_t = 0.01)]  beta:  f64,
    #[arg(long, default_value_t = 42)]    seed:  u64,
    #[arg(long, default_value_t = 200)]   iters: usize,
    #[arg(long, default_value_t = 10)]    top_n: usize,
}

#[derive(Args)]
pub struct PrimaryIdArgs {
    /// Primary record UUID.
    #[arg(short = 'p', long)]
    primary_id: String,
}

#[derive(Args)]
pub struct SecondaryIdArgs {
    /// Secondary record UUID.
    #[arg(short = 's', long)]
    secondary_id: String,
}

#[derive(Args)]
pub struct TsRangeArgs {
    #[arg(short = 's', long)]
    start_ts: u64,
    #[arg(short = 'e', long)]
    end_ts: u64,
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

        // ── Phase 6 file ingest ──────────────────────────────────────────
        Sub::AddFile(a) => {
            let mut params = json!({"path": a.path});
            if let Some(rf) = a.replication_factor {
                params["replication_factor"] = json!(rf);
            }
            crate::client::call(url, "v3/add.file", params)
        }
        Sub::AddFileSyslog(a) => {
            let mut params = json!({"path": a.path});
            if let Some(rf) = a.replication_factor {
                params["replication_factor"] = json!(rf);
            }
            crate::client::call(url, "v3/add.file.syslog", params)
        }

        // ── Phase 6 reads ────────────────────────────────────────────────
        Sub::Fulltext(a)       => crate::client::call(url, "v3/fulltext",
            json!({"query": a.query, "duration": a.duration, "limit": a.limit})),
        Sub::FulltextGet(a)    => crate::client::call(url, "v3/fulltext.get",
            json!({"query": a.query, "duration": a.duration})),
        Sub::FulltextRecent(a) => crate::client::call(url, "v3/fulltext.recent",
            json!({"query": a.query, "duration": a.duration, "limit": a.limit})),

        Sub::Keys(a)    => crate::client::call(url, "v3/keys",     json!({"duration": a.duration})),
        Sub::KeysAll(a) => crate::client::call(url, "v3/keys.all", json!({"duration": a.duration, "key": a.key})),
        Sub::KeysGet(a) => crate::client::call(url, "v3/keys.get", json!({"duration": a.duration, "key": a.key})),

        Sub::Primaries(a) => {
            let mut p = serde_json::Map::new();
            if let Some(d) = &a.duration { p.insert("duration".into(), json!(d)); }
            if let Some(s) = a.start_ts  { p.insert("start_ts".into(), json!(s)); }
            if let Some(e) = a.end_ts    { p.insert("end_ts".into(),   json!(e)); }
            crate::client::call(url, "v3/primaries", Value::Object(p))
        }
        Sub::PrimariesExplore(a) => crate::client::call(url, "v3/primaries.explore",
            json!({"duration": a.duration})),
        Sub::PrimariesExploreTelemetry(a) => crate::client::call(url, "v3/primaries.explore.telemetry",
            json!({"duration": a.duration})),
        Sub::PrimariesGet(a) => crate::client::call(url, "v3/primaries.get",
            json!({"duration": a.duration, "key": a.key})),
        Sub::PrimariesGetTelemetry(a) => crate::client::call(url, "v3/primaries.get.telemetry",
            json!({"duration": a.duration, "key": a.key})),

        Sub::Topics(a) => crate::client::call(url, "v3/topics", json!({
            "key": a.key, "duration": a.duration,
            "k": a.k, "alpha": a.alpha, "beta": a.beta, "seed": a.seed,
            "iters": a.iters, "top_n": a.top_n,
        })),
        Sub::TopicsAll(a) => crate::client::call(url, "v3/topics.all", json!({
            "duration": a.duration,
            "k": a.k, "alpha": a.alpha, "beta": a.beta, "seed": a.seed,
            "iters": a.iters, "top_n": a.top_n,
        })),

        Sub::Signals(a)      => crate::client::call(url, "v3/signals", json!({"duration": a.duration})),
        Sub::SignalsQuery(a) => crate::client::call(url, "v3/signals_query",
            json!({"query": a.query, "limit": a.limit})),

        Sub::SearchGet(a) => crate::client::call(url, "v3/search.get",
            json!({"query": a.query, "duration": a.duration, "limit": a.limit})),

        Sub::Secondaries(a) => crate::client::call(url, "v3/secondaries", json!({"primary_id": a.primary_id})),
        Sub::Secondary(a)   => crate::client::call(url, "v3/secondary",   json!({"secondary_id": a.secondary_id})),

        Sub::TplList(a)               => crate::client::call(url, "v3/tpl.list",   json!({"duration": a.duration})),
        Sub::TplSearch(a)             => crate::client::call(url, "v3/tpl.search",
            json!({"query": a.query, "duration": a.duration, "limit": a.limit})),
        Sub::TplGet(a)                => crate::client::call(url, "v3/tpl.get",            json!({"id": a.id})),
        Sub::TplTemplateById(a)       => crate::client::call(url, "v3/tpl.template_by_id", json!({"id": a.id})),
        Sub::TplTemplatesRecent(a)    => crate::client::call(url, "v3/tpl.templates_recent",
            json!({"duration": a.duration})),
        Sub::TplTemplatesByTimestamp(a) => crate::client::call(url, "v3/tpl.templates_by_timestamp",
            json!({"start_ts": a.start_ts, "end_ts": a.end_ts})),
    }
}
