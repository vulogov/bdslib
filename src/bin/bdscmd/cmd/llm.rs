//! `bdscmd llm <subcommand>` — drive the v4/llm.* surface from the shell.
//!
//! Mirrors the v4/* surface that bdsweb and `cls.llm.*` Bund words
//! already consume.  Every call is HMAC-signed under
//! `--secret` (or `BDSCMD_CLUSTER_SECRET`) matching `cluster.shared_secret`
//! in `bds.hjson` — v4/* refuses unsigned requests by design, so
//! `bdscmd llm` requires the secret on every subcommand (no
//! first-user-style bootstrap exists for the LLM surface).
//!
//! Subcommands:
//!
//! - Sync ops:        `complete`, `chat`, `analyze`, `embed`
//! - Discovery:       `providers`
//! - Async + jobs:    `async`, `status`, `cancel`, `jobs`
//! - Cache admin:     `cache stats`, `cache purge`

use anyhow::{bail, Context, Result};
use bdslib::cluster::hmac_auth;
use clap::{Args, Subcommand};
use serde_json::{json, Map, Value};
use std::fs;

#[derive(Args)]
pub struct Cmd {
    /// Cluster shared secret — same value as `cluster.shared_secret`
    /// in bds.hjson.  Required by every v4/llm.* method.  Pulled from
    /// `BDSCMD_CLUSTER_SECRET` when not supplied on the command line.
    #[arg(short, long, env = "BDSCMD_CLUSTER_SECRET", default_value = "")]
    secret: String,

    #[command(subcommand)]
    sub: Sub,
}

#[derive(Subcommand)]
enum Sub {
    /// Single-shot completion (`v4/llm.complete`).
    Complete(CompleteArgs),

    /// Send one turn in a stateful chat session (`v4/llm.chat`).
    /// Persists history in the docstore; omit `--chat-id` to open a
    /// new session.
    Chat(ChatArgs),

    /// Build a RAG context from bdslib data and run one completion
    /// over it (`v4/llm.analyze`).  Pick `--kind` to select the
    /// ContextSource variant.
    Analyze(AnalyzeArgs),

    /// Vector embeddings for one or more texts (`v4/llm.embed`).
    Embed(EmbedArgs),

    /// List registered providers + the default
    /// (`v4/llm.providers.list`).
    Providers,

    /// Enqueue a job for the background runner
    /// (`v4/llm.complete_async` or `v4/llm.analyze_async`).  Pick
    /// `--kind` = `complete` (default) or `analyze`.
    /// Returns `{job_id, result_id}` — poll with `bdscmd results-pull`.
    Async(AsyncArgs),

    /// Inspect a queued / in-flight / terminal job
    /// (`v4/llm.jobs.status`).
    Status(JobIdArgs),

    /// Cancel a pending or running job (`v4/llm.jobs.cancel`).
    /// Idempotent on terminal states.
    Cancel(JobIdArgs),

    /// List async jobs (`v4/llm.jobs.list`).
    Jobs(JobsArgs),

    /// Inference cache admin.
    #[command(subcommand)]
    Cache(CacheSub),
}

#[derive(Subcommand)]
enum CacheSub {
    /// Cache totals (`v4/llm.cache.stats`): enabled flag, ttl, row
    /// count, total hits, response bytes.
    Stats,

    /// Drop matching cache rows (`v4/llm.cache.purge`).  Empty filter
    /// set purges everything.
    Purge(PurgeArgs),
}

// ─────────────────────────────────────────────────────────────────────
// Per-subcommand argument structs
// ─────────────────────────────────────────────────────────────────────

#[derive(Args)]
struct CompleteArgs {
    /// Single-user-message prompt.  Mutually exclusive with `--messages-file`.
    #[arg(short = 'p', long, conflicts_with = "messages_file")]
    prompt: Option<String>,

    /// Read a JSON array of `[{role, content}, …]` messages from this
    /// file and forward verbatim as `messages`.
    #[arg(long, conflicts_with = "prompt")]
    messages_file: Option<String>,

    /// Override the provider name (registry key — `ollama`, `anthropic`, …).
    #[arg(long)]
    provider: Option<String>,

    /// Override the model.  Provider-specific id (e.g. `llama3.2`,
    /// `claude-sonnet-4-5`).  Falls back to the provider's default.
    #[arg(long)]
    model: Option<String>,

    /// `options.temperature`.  Setting > 0 disables caching for this call.
    #[arg(long)]
    temperature: Option<f32>,

    /// `options.max_tokens` cap on response length.
    #[arg(long)]
    max_tokens: Option<u32>,

    /// `options.top_p` nucleus sampling threshold.
    #[arg(long)]
    top_p: Option<f32>,

    /// `options.seed` (provider-dependent — Ollama / OpenAI honour;
    /// Anthropic ignores).
    #[arg(long)]
    seed: Option<u64>,

    /// Disable cache lookup AND store for this call only.  Implies
    /// `cache: false` in the request body.
    #[arg(long)]
    no_cache: bool,
}

#[derive(Args)]
struct ChatArgs {
    /// Existing chat session UUID.  Omit to open a fresh session;
    /// the response carries the new `chat_id`.
    #[arg(long)]
    chat_id: Option<String>,

    /// The operator's turn.  Required.
    #[arg(short = 'm', long, conflicts_with = "message_file")]
    message: Option<String>,

    /// Read the message body from a file (useful for long prompts /
    /// pre-built context strings).
    #[arg(long, conflicts_with = "message")]
    message_file: Option<String>,

    /// Lookback window for RAG context (humantime, e.g. `1h`).
    /// When set without `--context`, the server runs
    /// `db.aggregationsearch(duration, message)` and prepends the
    /// top-N fingerprints to the user message.
    #[arg(long)]
    duration: Option<String>,

    /// Verbatim RAG context that REPLACES the inline aggregation
    /// pass.  Provide as a string or via `--context-file`.
    #[arg(long, conflicts_with = "context_file")]
    context: Option<String>,

    #[arg(long, conflicts_with = "context")]
    context_file: Option<String>,

    /// System prompt seeded into a NEW session.  Ignored for
    /// follow-up turns (the session's stored system prompt wins).
    #[arg(long)]
    system_prompt: Option<String>,

    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
}

#[derive(Args)]
struct AnalyzeArgs {
    /// ContextSource variant.  One of: aggregation, knn, rca, anomaly,
    /// templates, telemetry, documents, supplied.
    #[arg(short = 'k', long)]
    kind: String,

    /// Humantime window (required by most kinds — anything except
    /// `documents` and `supplied`).
    #[arg(long)]
    duration: Option<String>,

    /// Operator's question.  Appended after the assembled context.
    #[arg(short = 'q', long)]
    query: Option<String>,

    /// Override the per-kind default preamble.
    #[arg(long)]
    prompt_template: Option<String>,

    /// `kind=knn`: number of neighbours.
    #[arg(long)]
    k: Option<usize>,

    /// `kind=templates`: how many top templates to surface.
    #[arg(long)]
    top_n: Option<usize>,

    /// `kind=anomaly` / `telemetry` / `aggregation`: row cap.
    #[arg(long)]
    limit: Option<usize>,

    /// `kind=rca`: hint at which `key` represents failure events.
    #[arg(long)]
    failure_key: Option<String>,

    /// `kind=documents`: comma-separated list of UUIDs (or repeat `--id`).
    #[arg(long, value_delimiter = ',')]
    id: Vec<String>,

    /// `kind=supplied`: path to a JSON file containing the row array.
    #[arg(long)]
    rows_file: Option<String>,

    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,

    /// `options.temperature`.
    #[arg(long)]
    temperature: Option<f32>,
    /// `options.max_tokens`.
    #[arg(long)]
    max_tokens: Option<u32>,

    /// Skip the cache for this call.
    #[arg(long)]
    no_cache: bool,
}

#[derive(Args)]
struct EmbedArgs {
    /// Single text to embed (forwards as `texts: [text]`).
    #[arg(short = 't', long, conflicts_with = "texts_file")]
    text: Option<String>,

    /// Read one text per line from a file; all lines forwarded
    /// as `texts: [...]`.
    #[arg(long, conflicts_with = "text")]
    texts_file: Option<String>,

    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
}

#[derive(Args)]
struct AsyncArgs {
    /// Job kind.  `complete` enqueues a completion (uses
    /// `--prompt` / `--messages-file` like `complete`);
    /// `analyze` enqueues an analyze job (uses `--analyze-kind`
    /// + `--query` + per-kind extras like `analyze`).
    #[arg(short = 'k', long, default_value = "complete")]
    kind: String,

    // complete variant fields
    #[arg(short = 'p', long, conflicts_with = "messages_file")]
    prompt: Option<String>,
    #[arg(long, conflicts_with = "prompt")]
    messages_file: Option<String>,

    // analyze variant fields
    /// For `--kind analyze`: which ContextSource variant.
    #[arg(long)]
    analyze_kind: Option<String>,
    #[arg(long)]
    duration: Option<String>,
    #[arg(short = 'q', long)]
    query: Option<String>,
    #[arg(long)]
    rows_file: Option<String>,
    #[arg(long, value_delimiter = ',')]
    id: Vec<String>,

    // shared options
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    temperature: Option<f32>,
    #[arg(long)]
    max_tokens: Option<u32>,

    /// Reuse an existing ResultQueue id rather than minting a new one
    /// (useful when fanning out several jobs to a single waiter).
    #[arg(long)]
    result_id: Option<String>,
}

#[derive(Args)]
struct JobIdArgs {
    #[arg(short = 'i', long)]
    job_id: String,
}

#[derive(Args)]
struct JobsArgs {
    /// Restrict to one state: pending | running | done | failed | cancelled.
    #[arg(long)]
    state: Option<String>,

    /// Soft row cap.
    #[arg(long, default_value_t = 100)]
    limit: u64,
}

#[derive(Args)]
struct PurgeArgs {
    /// Drop rows from this provider only.
    #[arg(long)]
    provider: Option<String>,

    /// Drop rows of this kind only (e.g. `complete`, `analyze:rca`).
    #[arg(long)]
    kind: Option<String>,

    /// Drop rows older than this many seconds.  Converted into the
    /// absolute `older_than_created` unix timestamp before being sent.
    #[arg(long)]
    older_than_secs: Option<u64>,
}

// ─────────────────────────────────────────────────────────────────────
// HMAC helper (same shape as cmd::user::signed_call)
// ─────────────────────────────────────────────────────────────────────

fn signed_call(
    url:     &str,
    method:  &str,
    secret:  &str,
    mut params: Map<String, Value>,
) -> Result<Value> {
    if secret.is_empty() {
        bail!("--secret is required for `llm {}` (or set BDSCMD_CLUSTER_SECRET) — \
               v4/llm.* refuses unsigned requests",
              method.trim_start_matches("v4/llm."));
    }
    let canonical = serde_json::to_vec(&Value::Object(params.clone()))
        .context("serialize params for HMAC")?;
    let sig = hmac_auth::sign(secret, &canonical);
    params.insert("_hmac".into(), Value::String(sig));
    crate::client::call(url, method, Value::Object(params))
}

fn require_text_input(prompt: Option<String>, prompt_file: Option<String>, what: &str)
    -> Result<Option<String>>
{
    match (prompt, prompt_file) {
        (Some(s), None)  => Ok(Some(s)),
        (None, Some(p))  => Ok(Some(fs::read_to_string(&p)
                                 .with_context(|| format!("read {what} from {p:?}"))?)),
        (None, None)     => Ok(None),
        (Some(_), Some(_)) => bail!("supplied both --{what} and --{what}-file (mutually exclusive)"),
    }
}

fn build_options(
    temperature: Option<f32>,
    max_tokens:  Option<u32>,
    top_p:       Option<f32>,
    seed:        Option<u64>,
) -> Option<Value> {
    let mut obj = Map::new();
    if let Some(t) = temperature { obj.insert("temperature".into(), json!(t)); }
    if let Some(m) = max_tokens  { obj.insert("max_tokens".into(),  json!(m)); }
    if let Some(p) = top_p       { obj.insert("top_p".into(),       json!(p)); }
    if let Some(s) = seed        { obj.insert("seed".into(),        json!(s)); }
    if obj.is_empty() { None } else { Some(Value::Object(obj)) }
}

fn maybe_insert<T: Into<Value>>(m: &mut Map<String, Value>, key: &str, v: Option<T>) {
    if let Some(x) = v { m.insert(key.into(), x.into()); }
}

fn maybe_insert_string(m: &mut Map<String, Value>, key: &str, v: Option<String>) {
    if let Some(s) = v { m.insert(key.into(), Value::String(s)); }
}

// ─────────────────────────────────────────────────────────────────────
// Dispatch
// ─────────────────────────────────────────────────────────────────────

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    let secret = args.secret;
    match args.sub {
        Sub::Complete(a) => run_complete(url, &secret, a),
        Sub::Chat(a)     => run_chat(url, &secret, a),
        Sub::Analyze(a)  => run_analyze(url, &secret, a),
        Sub::Embed(a)    => run_embed(url, &secret, a),
        Sub::Providers   => signed_call(url, "v4/llm.providers.list", &secret, Map::new()),
        Sub::Async(a)    => run_async(url, &secret, a),
        Sub::Status(a)   => signed_call(url, "v4/llm.jobs.status", &secret,
                                        once_map("job_id", a.job_id)),
        Sub::Cancel(a)   => signed_call(url, "v4/llm.jobs.cancel", &secret,
                                        once_map("job_id", a.job_id)),
        Sub::Jobs(a)     => run_jobs(url, &secret, a),
        Sub::Cache(c)    => match c {
            CacheSub::Stats     => signed_call(url, "v4/llm.cache.stats", &secret, Map::new()),
            CacheSub::Purge(p)  => run_cache_purge(url, &secret, p),
        },
    }
}

fn once_map(k: &str, v: String) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(k.into(), Value::String(v));
    m
}

// ── complete ────────────────────────────────────────────────────────

fn run_complete(url: &str, secret: &str, a: CompleteArgs) -> Result<Value> {
    let mut params = Map::new();
    if let Some(prompt) = a.prompt {
        params.insert("prompt".into(), Value::String(prompt));
    } else if let Some(path) = a.messages_file {
        let messages = read_json_file(&path, "messages file")?;
        params.insert("messages".into(), messages);
    } else {
        bail!("`llm complete` requires --prompt or --messages-file");
    }
    maybe_insert_string(&mut params, "provider", a.provider);
    maybe_insert_string(&mut params, "model",    a.model);
    if let Some(opts) = build_options(a.temperature, a.max_tokens, a.top_p, a.seed) {
        params.insert("options".into(), opts);
    }
    if a.no_cache { params.insert("cache".into(), Value::Bool(false)); }
    signed_call(url, "v4/llm.complete", secret, params)
}

// ── chat ────────────────────────────────────────────────────────────

fn run_chat(url: &str, secret: &str, a: ChatArgs) -> Result<Value> {
    let message = require_text_input(a.message, a.message_file, "message")?
        .ok_or_else(|| anyhow::anyhow!("`llm chat` requires --message or --message-file"))?;
    let context = require_text_input(a.context, a.context_file, "context")?;

    let mut params = Map::new();
    params.insert("message".into(), Value::String(message));
    maybe_insert_string(&mut params, "chat_id",       a.chat_id);
    maybe_insert_string(&mut params, "duration",      a.duration);
    maybe_insert_string(&mut params, "context",       context);
    maybe_insert_string(&mut params, "system_prompt", a.system_prompt);
    maybe_insert_string(&mut params, "provider",      a.provider);
    maybe_insert_string(&mut params, "model",         a.model);
    signed_call(url, "v4/llm.chat", secret, params)
}

// ── analyze ─────────────────────────────────────────────────────────

fn run_analyze(url: &str, secret: &str, a: AnalyzeArgs) -> Result<Value> {
    let mut params = Map::new();
    params.insert("kind".into(), Value::String(a.kind.clone()));
    maybe_insert_string(&mut params, "duration",        a.duration);
    maybe_insert_string(&mut params, "query",           a.query);
    maybe_insert_string(&mut params, "prompt_template", a.prompt_template);
    maybe_insert_string(&mut params, "failure_key",     a.failure_key);
    maybe_insert::<u64>(&mut params, "k",         a.k.map(|n| n as u64));
    maybe_insert::<u64>(&mut params, "top_n",     a.top_n.map(|n| n as u64));
    maybe_insert::<u64>(&mut params, "limit",     a.limit.map(|n| n as u64));
    maybe_insert_string(&mut params, "provider",  a.provider);
    maybe_insert_string(&mut params, "model",     a.model);
    if let Some(opts) = build_options(a.temperature, a.max_tokens, None, None) {
        params.insert("options".into(), opts);
    }
    if a.no_cache { params.insert("cache".into(), Value::Bool(false)); }

    if a.kind == "documents" {
        if a.id.is_empty() {
            bail!("`llm analyze --kind documents` requires at least one --id <UUID>");
        }
        params.insert("ids".into(), Value::Array(
            a.id.into_iter().map(Value::String).collect()));
    }
    if a.kind == "supplied" {
        let rows_file = a.rows_file
            .ok_or_else(|| anyhow::anyhow!("`llm analyze --kind supplied` requires --rows-file <JSON>"))?;
        let rows = read_json_file(&rows_file, "supplied rows")?;
        if !rows.is_array() {
            bail!("supplied rows file must contain a JSON array; got {}", short_kind(&rows));
        }
        params.insert("rows".into(), rows);
    }

    signed_call(url, "v4/llm.analyze", secret, params)
}

// ── embed ───────────────────────────────────────────────────────────

fn run_embed(url: &str, secret: &str, a: EmbedArgs) -> Result<Value> {
    let mut params = Map::new();
    if let Some(text) = a.text {
        params.insert("text".into(), Value::String(text));
    } else if let Some(path) = a.texts_file {
        let body = fs::read_to_string(&path)
            .with_context(|| format!("read texts file {path:?}"))?;
        let texts: Vec<Value> = body.lines()
            .map(|l| l.trim_end())
            .filter(|l| !l.is_empty())
            .map(|l| Value::String(l.to_owned()))
            .collect();
        if texts.is_empty() {
            bail!("texts file {path:?} contained no non-empty lines");
        }
        params.insert("texts".into(), Value::Array(texts));
    } else {
        bail!("`llm embed` requires --text or --texts-file");
    }
    maybe_insert_string(&mut params, "provider", a.provider);
    maybe_insert_string(&mut params, "model",    a.model);
    signed_call(url, "v4/llm.embed", secret, params)
}

// ── async ───────────────────────────────────────────────────────────

fn run_async(url: &str, secret: &str, a: AsyncArgs) -> Result<Value> {
    let (method, params) = match a.kind.as_str() {
        "complete" => {
            let mut p = Map::new();
            if let Some(prompt) = a.prompt {
                p.insert("prompt".into(), Value::String(prompt));
            } else if let Some(path) = a.messages_file {
                p.insert("messages".into(), read_json_file(&path, "messages file")?);
            } else {
                bail!("`llm async --kind complete` requires --prompt or --messages-file");
            }
            maybe_insert_string(&mut p, "provider",  a.provider);
            maybe_insert_string(&mut p, "model",     a.model);
            maybe_insert_string(&mut p, "result_id", a.result_id);
            if let Some(opts) = build_options(a.temperature, a.max_tokens, None, None) {
                p.insert("options".into(), opts);
            }
            ("v4/llm.complete_async", p)
        }
        "analyze" => {
            let analyze_kind = a.analyze_kind
                .ok_or_else(|| anyhow::anyhow!(
                    "`llm async --kind analyze` requires --analyze-kind"
                ))?;
            let mut p = Map::new();
            p.insert("kind".into(), Value::String(analyze_kind.clone()));
            maybe_insert_string(&mut p, "duration",  a.duration);
            maybe_insert_string(&mut p, "query",     a.query);
            maybe_insert_string(&mut p, "provider",  a.provider);
            maybe_insert_string(&mut p, "model",     a.model);
            maybe_insert_string(&mut p, "result_id", a.result_id);
            if !a.id.is_empty() {
                p.insert("ids".into(), Value::Array(
                    a.id.into_iter().map(Value::String).collect()));
            }
            if let Some(path) = a.rows_file {
                p.insert("rows".into(), read_json_file(&path, "supplied rows")?);
            }
            if let Some(opts) = build_options(a.temperature, a.max_tokens, None, None) {
                p.insert("options".into(), opts);
            }
            ("v4/llm.analyze_async", p)
        }
        other => bail!("unknown async kind {other:?} (expected `complete` or `analyze`)"),
    };
    signed_call(url, method, secret, params)
}

// ── jobs list ───────────────────────────────────────────────────────

fn run_jobs(url: &str, secret: &str, a: JobsArgs) -> Result<Value> {
    let mut params = Map::new();
    maybe_insert_string(&mut params, "state", a.state);
    params.insert("limit".into(), json!(a.limit));
    signed_call(url, "v4/llm.jobs.list", secret, params)
}

// ── cache purge ─────────────────────────────────────────────────────

fn run_cache_purge(url: &str, secret: &str, a: PurgeArgs) -> Result<Value> {
    let mut params = Map::new();
    maybe_insert_string(&mut params, "provider", a.provider);
    maybe_insert_string(&mut params, "kind",     a.kind);
    if let Some(secs) = a.older_than_secs {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);
        let cutoff = now.saturating_sub(secs);
        params.insert("older_than_created".into(), json!(cutoff));
    }
    signed_call(url, "v4/llm.cache.purge", secret, params)
}

// ─────────────────────────────────────────────────────────────────────
// Misc
// ─────────────────────────────────────────────────────────────────────

fn read_json_file(path: &str, what: &str) -> Result<Value> {
    let s = fs::read_to_string(path)
        .with_context(|| format!("read {what} from {path:?}"))?;
    serde_json::from_str(&s)
        .with_context(|| format!("parse {what} from {path:?}"))
}

fn short_kind(v: &Value) -> &'static str {
    match v {
        Value::Null      => "null",
        Value::Bool(_)   => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_)  => "array",
        Value::Object(_) => "object",
    }
}
