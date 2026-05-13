//! `bdscmd ask` — docstore-backed Q&A over the cluster (`v3/help`).
//!
//! Named `ask` rather than `help` because clap reserves `help` for
//! its auto-generated per-subcommand help surface (`bdscmd help status`
//! → docs for `status`); a custom `help` subcommand would clobber
//! that.  Semantically equivalent — the underlying RPC is `v3/help`.
//!
//! Hand an English question to the cluster's default LLM provider
//! with the docstore as RAG context.  Optional `--internal-only`
//! flag restricts retrieval to documents whose `metadata.internal_doc
//! == true` — the corpus loaded by
//! `scripts/load_internal_documentation.sh`.
//!
//! Wraps [`v3/help`](../../Documentation/jsonrpc_api/v3_help.md);
//! unauthenticated v3/* read surface, no HMAC required.

use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::{Map, Value};

#[derive(Args)]
pub struct Cmd {
    /// English question to ask.  Mutually exclusive with
    /// `--message-file`; if neither is given, the message is read
    /// from stdin.
    message: Option<String>,

    /// Read the question from a file (use `-` for stdin).  Convenient
    /// when the request is multi-line.
    #[arg(long, conflicts_with = "message")]
    message_file: Option<String>,

    /// Restrict RAG to documents tagged `metadata.internal_doc=true`
    /// (the corpus loaded by `scripts/load_internal_documentation.sh`).
    #[arg(long)]
    internal_only: bool,

    /// Number of documents to include in the prompt.  Server-clamped
    /// to `[1, 50]`; omit to use the server-side default (8).
    #[arg(short = 'l', long)]
    limit: Option<u64>,

    /// Override the cluster's default provider (`""` / omitted →
    /// `llm.default`).
    #[arg(long)]
    provider: Option<String>,

    /// Override the provider's default model.
    #[arg(long)]
    model: Option<String>,

    /// Sampling temperature passed through as `options.temperature`.
    #[arg(long)]
    temperature: Option<f64>,

    /// Hard cap on output tokens.
    #[arg(long)]
    max_tokens: Option<u64>,

    /// nucleus-sampling top-p.
    #[arg(long)]
    top_p: Option<f64>,

    /// Deterministic seed (Ollama / OpenAI only).
    #[arg(long)]
    seed: Option<u64>,

    /// Override the auto-bucketed context window (16k / 32k / 64k).
    #[arg(long)]
    num_ctx: Option<u64>,

    /// Pipeline mode — print just the answer body to stdout and a
    /// one-line "provider/model · N docs · ms" summary to stderr.
    /// Sources are also dumped to stderr (one per line) so a tee can
    /// capture them.  Exit code is `0` when at least one document
    /// fed the prompt, `2` otherwise.
    #[arg(long)]
    answer_only: bool,
}

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    let message = resolve_message(&args)?;

    let mut params = Map::new();
    params.insert("message".into(), Value::String(message));
    if args.internal_only {
        params.insert("internal_only".into(), Value::Bool(true));
    }
    if let Some(n) = args.limit {
        params.insert("limit".into(), Value::Number(n.into()));
    }
    if let Some(p) = args.provider.as_deref() {
        if !p.is_empty() { params.insert("provider".into(), Value::String(p.to_owned())); }
    }
    if let Some(m) = args.model.as_deref() {
        if !m.is_empty() { params.insert("model".into(), Value::String(m.to_owned())); }
    }
    if let Some(opts) = build_options(
        args.temperature, args.max_tokens, args.top_p, args.seed, args.num_ctx,
    ) {
        params.insert("options".into(), opts);
    }

    let resp = crate::client::call(url, "v3/help", Value::Object(params))?;

    if args.answer_only {
        let answer  = resp.get("answer").and_then(|v| v.as_str()).unwrap_or("");
        let n_docs  = resp.get("n_docs").and_then(|v| v.as_u64()).unwrap_or(0);
        let ms      = resp.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let provider = resp.get("provider").and_then(|v| v.as_str()).unwrap_or("?");
        let model    = resp.get("model").and_then(|v| v.as_str()).unwrap_or("?");

        print!("{answer}");
        if !answer.ends_with('\n') { println!(); }

        eprintln!("[help] {provider}/{model} · {n_docs} doc(s) · {ms}ms");
        if let Some(sources) = resp.get("sources").and_then(|v| v.as_array()) {
            for s in sources {
                let name  = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let score = s.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let internal = s.get("internal_doc").and_then(|v| v.as_bool()).unwrap_or(false);
                let badge = if internal { "[I]" } else { "[ ]" };
                eprintln!("[help] {badge} {score:.3}  {name}");
            }
        }
        if let Some(note) = resp.get("note").and_then(|v| v.as_str()) {
            if !note.is_empty() { eprintln!("[help] note: {note}"); }
        }

        // Exit before main.rs prints the JSON envelope.  Non-zero when
        // the answer was produced WITHOUT any docs (general-knowledge
        // fallback) so pipelines can branch on `$?`.
        std::process::exit(if resp.get("n_docs").and_then(|v| v.as_u64()).unwrap_or(0) > 0 { 0 } else { 2 });
    }

    Ok(resp)
}

fn resolve_message(args: &Cmd) -> Result<String> {
    if let Some(s) = &args.message {
        return Ok(s.clone());
    }
    let raw = match args.message_file.as_deref() {
        Some("-") | None => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .context("failed to read question from stdin")?;
            buf
        }
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read message file {path}"))?,
    };
    let trimmed = raw.trim().to_owned();
    if trimmed.is_empty() {
        bail!("`help` requires a non-empty question (positional, --message-file, or stdin)");
    }
    Ok(trimmed)
}

fn build_options(
    temperature: Option<f64>,
    max_tokens:  Option<u64>,
    top_p:       Option<f64>,
    seed:        Option<u64>,
    num_ctx:     Option<u64>,
) -> Option<Value> {
    let mut obj = Map::new();
    if let Some(t) = temperature {
        obj.insert("temperature".into(),
            Value::Number(serde_json::Number::from_f64(t).unwrap_or(0.into())));
    }
    if let Some(n) = max_tokens { obj.insert("max_tokens".into(), Value::Number(n.into())); }
    if let Some(p) = top_p {
        obj.insert("top_p".into(),
            Value::Number(serde_json::Number::from_f64(p).unwrap_or(0.into())));
    }
    if let Some(s) = seed    { obj.insert("seed".into(),    Value::Number(s.into())); }
    if let Some(n) = num_ctx { obj.insert("num_ctx".into(), Value::Number(n.into())); }
    if obj.is_empty() { None } else { Some(Value::Object(obj)) }
}
