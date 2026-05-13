//! `bdscmd to-bund` — English → Bund translator (`v2/to.bund`).
//!
//! Hands a natural-language request to the cluster's default LLM
//! provider, validates the returned Bund script, and prints the
//! result.  Default output is the full Translation JSON (matching
//! every other `bdscmd` subcommand); add `--script-only` to print
//! just the generated script to stdout so the output is easy to
//! pipe into `bdscmd eval`:
//!
//! ```text
//! bdscmd to-bund --script-only "print 42" | bdscmd eval -
//! ```
//!
//! Validation summary always goes to stderr when `--script-only` is
//! set so users can confirm the parse attempts / provider / model /
//! ms without disturbing the stdout pipeline.
//!
//! Like the other v2/* commands, this is unsigned — the bdsnode RPC
//! port is the trust boundary, not an HMAC header.

use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::{Map, Value};

#[derive(Args)]
pub struct Cmd {
    /// English request to translate.  Mutually exclusive with
    /// `--message-file`; if neither is given, the message is read
    /// from stdin.
    message: Option<String>,

    /// Read the English request from a file (use `-` for stdin).
    /// Convenient when the request is multi-line and shell quoting
    /// gets in the way.
    #[arg(long, conflicts_with = "message")]
    message_file: Option<String>,

    /// Override the cluster's default provider (`""` / omitted →
    /// `llm.default`).
    #[arg(long)]
    provider: Option<String>,

    /// Override the provider's default model (`""` / omitted →
    /// provider's `default_model`).
    #[arg(long)]
    model: Option<String>,

    /// Override `llm.to_bund.max_retries` for this call.  Server
    /// caps at 5 — higher values are silently clamped.
    #[arg(long)]
    max_retries: Option<u64>,

    /// Sampling temperature passed through to the provider.
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

    /// Larger context window for the LLM.  The server picks a sane
    /// default based on prompt size; override only when the request
    /// + extras blow past that bucket.
    #[arg(long)]
    num_ctx: Option<u64>,

    /// Print just the Bund script to stdout (no JSON envelope).
    /// Translation metadata is sent to stderr so pipelines stay
    /// clean: `bdscmd to-bund --script-only "…" > /tmp/x.bund`.
    #[arg(long)]
    script_only: bool,
}

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    let message = resolve_message(&args)?;

    let mut params = Map::new();
    params.insert("message".into(), Value::String(message));
    if let Some(p) = args.provider.as_deref() {
        if !p.is_empty() { params.insert("provider".into(), Value::String(p.to_owned())); }
    }
    if let Some(m) = args.model.as_deref() {
        if !m.is_empty() { params.insert("model".into(), Value::String(m.to_owned())); }
    }
    if let Some(n) = args.max_retries {
        params.insert("max_retries".into(), Value::Number(n.into()));
    }
    if let Some(opts) = build_options(
        args.temperature, args.max_tokens, args.top_p, args.seed, args.num_ctx,
    ) {
        params.insert("options".into(), opts);
    }

    let resp = crate::client::call(url, "v2/to.bund", Value::Object(params))?;

    if args.script_only {
        // Pipeline-friendly mode: stdout = script body only;
        // stderr = one-line summary so the user can sanity-check
        // what they're piping into `bdscmd eval`.
        let script = resp.get("script").and_then(|v| v.as_str()).unwrap_or("");
        let valid  = resp.get("valid").and_then(|v| v.as_bool()).unwrap_or(false);
        // Don't add a trailing newline — print! preserves the
        // server's own line endings exactly.  Most generators emit
        // a trailing newline anyway.
        print!("{script}");
        if !script.ends_with('\n') { println!(); }

        let provider = resp.get("provider").and_then(|v| v.as_str()).unwrap_or("?");
        let model    = resp.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        let attempts = resp.get("parse_attempts").and_then(|v| v.as_u64()).unwrap_or(0);
        let ms       = resp.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let verdict  = if valid { "ok" } else { "FAILED" };
        eprintln!("[to-bund] {verdict} · {provider}/{model} · attempts={attempts} · {ms}ms");
        if let Some(err) = resp.get("parse_error").and_then(|v| v.as_str()) {
            eprintln!("[to-bund] last validation error: {err}");
        }

        // Exit before main pretty-prints the JSON envelope.  Using
        // a non-zero code when the translation came back invalid
        // mirrors `bdscmd eval` semantics — pipelines can branch
        // on `$?` without parsing JSON.
        std::process::exit(if valid { 0 } else { 2 });
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
                .context("failed to read English message from stdin")?;
            buf
        }
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read message file {path}"))?,
    };
    let trimmed = raw.trim().to_owned();
    if trimmed.is_empty() {
        bail!("`to-bund` requires a non-empty English message (positional, --message-file, or stdin)");
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

