use bdslib::vm::helpers::file_helper::get_snippet;
use bdslib::vm::helpers::print_error::print_error_plain;
use bdslib::{
    bund_eval, dbpath_from_config, get_db, init_db, sync_db, LdaConfig, LogFormat,
    TelemetryTrend, TopicSummary,
};
use clap::{Parser, Subcommand};
use std::process;

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name    = "bdscli",
    about   = "BDS command-line interface",
    version,
    propagate_version = true
)]
struct Cli {
    /// Path to the hjson configuration file.
    /// Falls back to the BDS_CONFIG environment variable when omitted.
    #[arg(short, long, env = "BDS_CONFIG", global = true)]
    config: Option<String>,

    /// Suppress ANSI colour codes in error output.
    #[arg(long, global = true, default_value_t = false)]
    nocolor: bool,

    /// Log verbosity (0=env default, 1=info, 2=debug, 3=trace).
    #[arg(long, global = true, default_value_t = 0)]
    debug: u32,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Evaluate a BUND script.
    Eval {
        /// Read script from stdin.
        #[arg(long, conflicts_with_all = ["eval", "file", "url"])]
        stdin: bool,

        /// Evaluate an inline BUND expression.
        #[arg(short, long, conflicts_with_all = ["stdin", "file", "url"])]
        eval: Option<String>,

        /// Read script from a local file path.
        #[arg(short, long, conflicts_with_all = ["stdin", "eval", "url"])]
        file: Option<String>,

        /// Read script from a URL (http/https/ftp/file).
        #[arg(short, long, conflicts_with_all = ["stdin", "eval", "file"])]
        url: Option<String>,
    },

    /// Run analytical computations over a time-windowed key corpus.
    Analyze {
        #[command(subcommand)]
        mode: AnalyzeMode,
    },

    /// Generate synthetic documents and print them as JSON (or ingest into DB).
    Generate {
        /// Fraction of generated documents to re-emit as exact duplicates
        /// (same key and data, different timestamp).  Range: 0.0–1.0.
        /// E.g. 0.2 adds 20 duplicate records for every 100 generated.
        #[arg(long, default_value_t = 0.0)]
        duplicate: f64,

        #[command(subcommand)]
        mode: GenerateMode,
    },

    /// Search the document store.
    Search {
        #[command(subcommand)]
        mode: SearchMode,
    },

    /// Query documents from the store.
    Get {
        /// Time window to scan (e.g. `1h`, `30min`).
        /// When omitted all shards are scanned.
        #[arg(short, long)]
        duration: Option<String>,

        /// Return only primary records.
        #[arg(long, conflicts_with_all = ["secondary", "duplication_timestamps"])]
        primary: bool,

        /// Return secondaries for the primary given by --primary-id.
        #[arg(long, conflicts_with_all = ["primary", "duplication_timestamps"], requires = "primary_id")]
        secondary: bool,

        /// Show exact-match deduplication timestamps.
        /// Without --primary-id: list every primary that has duplicates with
        ///   its UUID, key, and the timestamps of each duplicate submission.
        /// With --primary-id: list only the duplicate timestamps for that record.
        #[arg(long, conflicts_with_all = ["primary", "secondary"])]
        duplication_timestamps: bool,

        /// UUID of the primary to scope --secondary or --duplication-timestamps.
        #[arg(long)]
        primary_id: Option<String>,
    },

    /// Flush all open shards to disk.
    Sync,

    /// Open (or create) the DB described by the config.
    /// With --new the existing DB directory is wiped first.
    Init {
        /// Remove the existing DB directory before initialising.
        #[arg(long, default_value_t = false)]
        new: bool,
    },
}

/// Log-entry format selector for `generate log`.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum LogFormatArg {
    /// Pick a format at random for each document (default).
    Random,
    /// RFC-3164 syslog style.
    Syslog,
    /// Apache Combined Log Format.
    Http,
    /// Nginx access log format.
    #[value(name = "http-nginx")]
    HttpNginx,
    /// Python exception traceback.
    Traceback,
}

#[derive(Subcommand)]
enum GenerateMode {
    /// Generate syslog / HTTP / traceback log-entry documents.
    Log {
        /// Time window for generated timestamps (humantime, e.g. `1h`, `30min`).
        #[arg(short, long, default_value = "1h")]
        duration: String,

        /// Number of documents to generate.
        #[arg(short = 'n', long, default_value_t = 100)]
        count: usize,

        /// Log-entry format to produce (default: random per document).
        #[arg(short, long, default_value = "random")]
        format: LogFormatArg,

        /// Ingest the generated documents into the DB instead of printing them.
        #[arg(long, default_value_t = false)]
        ingest: bool,
    },

    /// Generate metric telemetry documents with dotted keys and numeric values.
    Telemetry {
        /// Time window for generated timestamps (humantime, e.g. `1h`, `30min`).
        #[arg(short, long, default_value = "1h")]
        duration: String,

        /// Number of documents to generate.
        #[arg(short = 'n', long, default_value_t = 100)]
        count: usize,

        /// Restrict output to a specific metric key (e.g. `cpu.usage`).
        /// When omitted a random metric is chosen per document.
        #[arg(short, long)]
        key: Option<String>,

        /// Ingest the generated documents into the DB instead of printing them.
        #[arg(long, default_value_t = false)]
        ingest: bool,
    },

    /// Generate a mix of telemetry and log-entry documents.
    Mixed {
        /// Time window for generated timestamps (humantime, e.g. `1h`, `30min`).
        #[arg(short, long, default_value = "1h")]
        duration: String,

        /// Number of documents to generate.
        #[arg(short = 'n', long, default_value_t = 100)]
        count: usize,

        /// Fraction of documents that are telemetry (0.0 = all logs, 1.0 = all telemetry).
        #[arg(short, long, default_value_t = 0.5)]
        ratio: f64,

        /// Ingest the generated documents into the DB instead of printing them.
        #[arg(long, default_value_t = false)]
        ingest: bool,
    },

    /// Generate documents from a custom JSON template with $placeholder substitution.
    Templated {
        /// Time window for generated timestamps (humantime, e.g. `1h`, `30min`).
        #[arg(short, long, default_value = "1h")]
        duration: String,

        /// Number of documents to generate.
        #[arg(short = 'n', long, default_value_t = 100)]
        count: usize,

        /// Inline JSON template string.
        /// Use $timestamp, $int(min,max), $float(min,max), $choice(a,b,c),
        /// $bool, $uuid, $ip, $word, $name as placeholders.
        #[arg(long, conflicts_with = "template_file")]
        template: Option<String>,

        /// Path to a file containing the JSON template.
        #[arg(long, conflicts_with = "template")]
        template_file: Option<String>,

        /// Ingest the generated documents into the DB instead of printing them.
        #[arg(long, default_value_t = false)]
        ingest: bool,
    },
}

#[derive(Subcommand)]
enum AnalyzeMode {
    /// Compute telemetry trend statistics for a key over a time window.
    Trend {
        /// Metric key to analyse (e.g. `cpu.usage`).
        #[arg(short, long)]
        key: String,

        /// Lookback duration in humantime notation (e.g. `1h`, `30min`, `7days`).
        /// Ignored when --start and --end are both supplied.
        #[arg(short, long, default_value = "1h")]
        duration: String,

        /// Absolute window start (Unix seconds).  Must be paired with --end.
        #[arg(long, requires = "end")]
        start: Option<u64>,

        /// Absolute window end (Unix seconds).  Must be paired with --start.
        #[arg(long, requires = "start")]
        end: Option<u64>,
    },

    /// Run LDA topic modelling for a key and print the discovered keywords.
    Topics {
        /// Metric key to analyse.
        #[arg(short, long)]
        key: String,

        /// Lookback duration in humantime notation.
        /// Ignored when --start and --end are both supplied.
        #[arg(short, long, default_value = "1h")]
        duration: String,

        /// Absolute window start (Unix seconds).  Must be paired with --end.
        #[arg(long, requires = "end")]
        start: Option<u64>,

        /// Absolute window end (Unix seconds).  Must be paired with --start.
        #[arg(long, requires = "start")]
        end: Option<u64>,

        /// Number of topics.
        #[arg(long, default_value_t = 3)]
        k: usize,

        /// Number of Gibbs sampling iterations.
        #[arg(long, default_value_t = 200)]
        iters: usize,

        /// Top words extracted per topic.
        #[arg(long, default_value_t = 10)]
        top_n: usize,

        /// Dirichlet prior for document-topic distributions.
        #[arg(long, default_value_t = 0.1)]
        alpha: f64,

        /// Dirichlet prior for topic-word distributions.
        #[arg(long, default_value_t = 0.01)]
        beta: f64,

        /// RNG seed for reproducible runs.
        #[arg(long, default_value_t = 42)]
        seed: u64,
    },
}

#[derive(Subcommand)]
enum SearchMode {
    /// Full-text keyword search.
    Fts {
        /// Tantivy query string (e.g. `cpu AND usage`, `"disk full"`).
        #[arg(short, long)]
        query: String,

        /// Lookback duration in humantime notation.
        #[arg(short, long, default_value = "1h")]
        duration: String,

        /// Maximum number of results to display.
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },

    /// Semantic vector search.
    Vector {
        /// Free-form description of what you are looking for.
        #[arg(short, long)]
        query: String,

        /// Lookback duration in humantime notation.
        #[arg(short, long, default_value = "1h")]
        duration: String,

        /// Maximum number of results to display.
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    bdslib::setloglevel::setloglevel(cli.debug);

    if let Err(e) = run(cli) {
        print_error_plain(e);
        process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), easy_error::Error> {
    match cli.command {
        Command::Eval { stdin, eval, file, url } => {
            cmd_eval(cli.config.as_deref(), stdin, eval, file, url)
        }
        Command::Analyze { mode } => {
            setup_db(cli.config.as_deref())?;
            match mode {
                AnalyzeMode::Trend { key, duration, start, end } => {
                    cmd_trend(&key, &duration, start, end)
                }
                AnalyzeMode::Topics { key, duration, start, end, k, iters, top_n, alpha, beta, seed } => {
                    cmd_topics(&key, &duration, start, end, LdaConfig { k, iters, top_n, alpha, beta, seed })
                }
            }
        }
        Command::Generate { duplicate, mode } => {
            cmd_generate(cli.config.as_deref(), duplicate, mode)
        }
        Command::Search { mode } => {
            setup_db(cli.config.as_deref())?;
            cmd_search(mode)
        }
        Command::Get { duration, primary, secondary, duplication_timestamps, primary_id } => {
            setup_db(cli.config.as_deref())?;
            cmd_get(duration, primary, secondary, duplication_timestamps, primary_id)
        }
        Command::Sync => {
            setup_db(cli.config.as_deref())?;
            cmd_sync()
        }
        Command::Init { new } => {
            cmd_init(cli.config.as_deref(), new)
        }
    }
}

// ── shared setup ──────────────────────────────────────────────────────────────

fn setup_db(config: Option<&str>) -> Result<(), easy_error::Error> {
    init_db(config)
        .map_err(|e| easy_error::err_msg(format!("DB init failed: {e}")))?;
    bdslib::init_adam()
        .map_err(|e| easy_error::err_msg(format!("VM init failed: {e}")))?;
    bdslib::context::init(config)
        .map_err(|e| easy_error::err_msg(format!("BUND context init failed: {e}")))?;
    Ok(())
}

// ── eval ──────────────────────────────────────────────────────────────────────

fn cmd_eval(config: Option<&str>, stdin: bool, eval: Option<String>, file: Option<String>, url: Option<String>) -> Result<(), easy_error::Error> {
    bdslib::init_adam()
        .map_err(|e| easy_error::err_msg(format!("VM init failed: {e}")))?;
    bdslib::context::init(config)
        .map_err(|e| easy_error::err_msg(format!("BUND context init failed: {e}")))?;
    let snippet = match get_snippet(stdin, eval, file, url) {
        Some(s) => s,
        None => {
            return Err(easy_error::err_msg(
                "no script source: supply --stdin, --eval, --file, or --url",
            ));
        }
    };
    bund_eval(&snippet)
}

// ── trend ─────────────────────────────────────────────────────────────────────

fn cmd_trend(
    key: &str,
    duration: &str,
    start: Option<u64>,
    end: Option<u64>,
) -> Result<(), easy_error::Error> {
    let t = match (start, end) {
        (Some(s), Some(e)) => TelemetryTrend::query(key, s, e)?,
        _ => TelemetryTrend::query_window(key, duration)?,
    };

    println!("key        : {}", t.key);
    println!("window     : [{}, {})", t.start, t.end);
    println!("samples    : {}", t.n);

    if t.n == 0 {
        println!("(no data found)");
        return Ok(());
    }

    println!("min / max  : {:.6} / {:.6}", t.min, t.max);
    println!("mean       : {:.6}", t.mean);
    println!("median     : {:.6}", t.median);
    println!("std_dev    : {:.6}", t.std_dev);
    println!("variability: {:.6}  (CV)", t.variability);

    if t.anomalies.is_empty() {
        println!("anomalies  : none");
    } else {
        println!("anomalies  : {} flagged", t.anomalies.len());
        for p in &t.anomalies {
            println!("  [{}]  ts={}  value={:.6}", p.index, p.timestamp, p.value);
        }
    }

    if t.breakouts.is_empty() {
        println!("breakouts  : none");
    } else {
        println!("breakouts  : {} detected", t.breakouts.len());
        for p in &t.breakouts {
            println!("  [{}]  ts={}  value={:.6}", p.index, p.timestamp, p.value);
        }
    }

    Ok(())
}

// ── topics ────────────────────────────────────────────────────────────────────

fn cmd_topics(
    key: &str,
    duration: &str,
    start: Option<u64>,
    end: Option<u64>,
    config: LdaConfig,
) -> Result<(), easy_error::Error> {
    let s = match (start, end) {
        (Some(s), Some(e)) => TopicSummary::query(key, s, e, config)?,
        _ => TopicSummary::query_window(key, duration, config)?,
    };

    println!("key      : {}", s.key);
    println!("window   : [{}, {})", s.start, s.end);
    println!("docs     : {}", s.n_docs);
    println!("topics   : {}", s.n_topics);
    println!("keywords : {}", s.keywords);

    Ok(())
}

// ── search ────────────────────────────────────────────────────────────────────

fn cmd_search(mode: SearchMode) -> Result<(), easy_error::Error> {
    let db = get_db()?;
    match mode {
        SearchMode::Fts { query, duration, limit } => {
            let results = db.search_fts(&duration, &query)?;
            let shown = results.len().min(limit);
            println!("fts query  : {query:?}");
            println!("duration   : {duration}");
            println!("hits       : {}  (showing {})", results.len(), shown);
            for doc in results.iter().take(shown) {
                print_doc(doc);
            }
        }
        SearchMode::Vector { query, duration, limit } => {
            let q = serde_json::json!({ "data": query });
            let results = db.search_vector(&duration, &q)?;
            let shown = results.len().min(limit);
            println!("vector query : {query:?}");
            println!("duration     : {duration}");
            println!("hits         : {}  (showing {})", results.len(), shown);
            for doc in results.iter().take(shown) {
                print_doc(doc);
            }
        }
    }
    Ok(())
}

fn print_doc(doc: &serde_json::Value) {
    let key   = doc["key"].as_str().unwrap_or("?");
    let ts    = doc["timestamp"].as_u64().unwrap_or(0);
    let score = doc.get("_score").and_then(|v| v.as_f64());
    match score {
        Some(s) => println!("  [{ts}]  score={s:.4}  key={key}"),
        None    => println!("  [{ts}]  key={key}"),
    }
}

// ── generate ──────────────────────────────────────────────────────────────────

/// Return the duration string from any GenerateMode without consuming it.
fn mode_duration(mode: &GenerateMode) -> &str {
    match mode {
        GenerateMode::Log       { duration, .. } => duration,
        GenerateMode::Telemetry { duration, .. } => duration,
        GenerateMode::Mixed     { duration, .. } => duration,
        GenerateMode::Templated { duration, .. } => duration,
    }
}

/// Append `(docs.len() * pct).round()` duplicate records to `docs`.
/// Each duplicate copies the `key` and `data` of a randomly chosen source
/// document and receives a new timestamp within `[now - duration_secs, now]`.
fn inject_duplicates(docs: &mut Vec<serde_json::Value>, pct: f64, duration_secs: u64) {
    if pct <= 0.0 || docs.is_empty() {
        return;
    }
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let dup_count = ((docs.len() as f64) * pct.min(1.0)).round() as usize;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let n_src = docs.len();
    for _ in 0..dup_count {
        let mut dup = docs[rng.gen_range(0..n_src)].clone();
        let ts = now_secs.saturating_sub(rng.gen_range(0..=duration_secs));
        if let Some(obj) = dup.as_object_mut() {
            obj.insert("timestamp".to_string(), serde_json::json!(ts));
        }
        docs.push(dup);
    }
}

fn cmd_generate(config: Option<&str>, duplicate: f64, mode: GenerateMode) -> Result<(), easy_error::Error> {
    if !(0.0..=1.0).contains(&duplicate) {
        return Err(easy_error::err_msg("--duplicate must be between 0.0 and 1.0"));
    }

    let duration_secs = humantime::parse_duration(mode_duration(&mode))
        .map(|d| d.as_secs())
        .unwrap_or(3600);

    let (mut docs, ingest) = match mode {
        GenerateMode::Log { duration, count, format, ingest } => {
            let fmt = match format {
                LogFormatArg::Random    => LogFormat::Random,
                LogFormatArg::Syslog    => LogFormat::Syslog,
                LogFormatArg::Http      => LogFormat::Http,
                LogFormatArg::HttpNginx => LogFormat::HttpNginx,
                LogFormatArg::Traceback => LogFormat::Traceback,
            };
            let docs = bdslib::Generator::new().with_log_format(fmt).log_entries(&duration, count);
            (docs, ingest)
        }
        GenerateMode::Telemetry { duration, count, key, ingest } => {
            let mut g = bdslib::Generator::new();
            if let Some(k) = key { g = g.with_key(k); }
            let docs = g.telemetry(&duration, count);
            (docs, ingest)
        }
        GenerateMode::Mixed { duration, count, ratio, ingest } => {
            let docs = bdslib::Generator::new().mixed(&duration, count, ratio);
            (docs, ingest)
        }
        GenerateMode::Templated { duration, count, template, template_file, ingest } => {
            let tmpl = resolve_template(template, template_file)?;
            let docs = bdslib::Generator::new().templated(&duration, &tmpl, count);
            (docs, ingest)
        }
    };

    inject_duplicates(&mut docs, duplicate, duration_secs);

    emit_or_ingest(config, docs, ingest)
}

fn resolve_template(
    inline: Option<String>,
    path: Option<String>,
) -> Result<String, easy_error::Error> {
    if let Some(t) = inline {
        return Ok(t);
    }
    if let Some(p) = path {
        return std::fs::read_to_string(&p)
            .map_err(|e| easy_error::err_msg(format!("cannot read template file {p:?}: {e}")));
    }
    Err(easy_error::err_msg(
        "templated: supply --template or --template-file",
    ))
}

fn emit_or_ingest(
    config: Option<&str>,
    docs: Vec<serde_json::Value>,
    ingest: bool,
) -> Result<(), easy_error::Error> {
    if ingest {
        setup_db(config)?;
        let db = get_db()?;
        let n = docs.len();
        db.add_batch(docs)
            .map_err(|e| easy_error::err_msg(format!("ingest failed: {e}")))?;
        sync_db().map_err(|e| easy_error::err_msg(format!("sync failed: {e}")))?;
        println!("ingested: {n}");
    } else {
        for doc in &docs {
            println!("{}", doc);
        }
    }
    Ok(())
}

// ── sync ──────────────────────────────────────────────────────────────────────

fn cmd_sync() -> Result<(), easy_error::Error> {
    sync_db()?;
    println!("sync: OK");
    Ok(())
}

// ── init ──────────────────────────────────────────────────────────────────────

fn cmd_init(config: Option<&str>, recreate: bool) -> Result<(), easy_error::Error> {
    if recreate {
        let dbpath = dbpath_from_config(config)
            .map_err(|e| easy_error::err_msg(format!("config error: {e}")))?;
        let p = std::path::Path::new(&dbpath);
        if p.exists() {
            std::fs::remove_dir_all(p)
                .map_err(|e| easy_error::err_msg(format!("cannot remove {dbpath:?}: {e}")))?;
            println!("removed: {dbpath}");
        }
    }
    setup_db(config)?;
    println!("init: OK");
    Ok(())
}

// ── get ───────────────────────────────────────────────────────────────────────

fn cmd_get(
    duration: Option<String>,
    primary: bool,
    secondary: bool,
    duplication_timestamps: bool,
    primary_id: Option<String>,
) -> Result<(), easy_error::Error> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    // Runtime validation: --primary-id is only meaningful with --secondary or
    // --duplication-timestamps.
    if primary_id.is_some() && !secondary && !duplication_timestamps {
        return Err(easy_error::err_msg(
            "--primary-id requires --secondary or --duplication-timestamps",
        ));
    }

    let db = get_db()?;
    let cache = db.cache();
    let info = cache.info();

    // ── duplication-timestamps mode ───────────────────────────────────────────
    if duplication_timestamps {
        let all_infos = info.list_all()
            .map_err(|e| easy_error::err_msg(format!("catalog error: {e}")))?;

        if let Some(ref raw) = primary_id {
            // Scoped to one primary
            let pid = Uuid::parse_str(raw)
                .map_err(|e| easy_error::err_msg(format!("invalid UUID {raw:?}: {e}")))?;

            let mut found = false;
            for si in &all_infos {
                let shard = cache.shard(si.start_time)
                    .map_err(|e| easy_error::err_msg(format!("shard error: {e}")))?;
                let times = shard.observability()
                    .get_duplicate_timestamps_by_id(pid)
                    .map_err(|e| easy_error::err_msg(format!("query error: {e}")))?;
                if !times.is_empty() {
                    let ts_list: Vec<u64> = times.iter()
                        .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
                        .collect();
                    println!("{}", serde_json::json!({
                        "primary_id": raw,
                        "duplicate_timestamps": ts_list,
                    }));
                    found = true;
                    break;
                }
            }
            if !found {
                log::debug!("no duplicate timestamps for {raw}");
            }
        } else {
            // All primaries across all shards that have duplicates
            let mut total = 0usize;
            for si in &all_infos {
                let shard = cache.shard(si.start_time)
                    .map_err(|e| easy_error::err_msg(format!("shard error: {e}")))?;
                let entries = shard.observability()
                    .list_all_dedup_entries()
                    .map_err(|e| easy_error::err_msg(format!("query error: {e}")))?;
                for (id, key, times) in entries {
                    let ts_list: Vec<u64> = times.iter()
                        .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
                        .collect();
                    println!("{}", serde_json::json!({
                        "primary_id": id.to_string(),
                        "key": key,
                        "duplicate_timestamps": ts_list,
                    }));
                    total += 1;
                }
            }
            log::debug!("dedup entries: {total}");
        }
        return Ok(());
    }

    // ── secondary mode ────────────────────────────────────────────────────────
    if secondary {
        let raw = primary_id.unwrap();
        let pid = Uuid::parse_str(&raw)
            .map_err(|e| easy_error::err_msg(format!("invalid UUID {raw:?}: {e}")))?;

        let all_infos = info.list_all()
            .map_err(|e| easy_error::err_msg(format!("catalog error: {e}")))?;

        for si in &all_infos {
            let shard = cache.shard(si.start_time)
                .map_err(|e| easy_error::err_msg(format!("shard error: {e}")))?;
            let obs = shard.observability();

            if obs.is_primary(pid).unwrap_or(false) {
                let sec_ids = obs.list_secondaries(pid)
                    .map_err(|e| easy_error::err_msg(format!("query error: {e}")))?;
                let n = sec_ids.len();
                for sid in sec_ids {
                    if let Some(doc) = obs.get_by_id(sid)
                        .map_err(|e| easy_error::err_msg(format!("fetch error: {e}")))?
                    {
                        println!("{doc}");
                    }
                }
                log::debug!("secondaries: {n}");
                return Ok(());
            }
        }
        return Err(easy_error::err_msg(format!("primary {raw} not found in any shard")));
    }

    // ── primary / all-records modes ───────────────────────────────────────────
    let (shard_infos, start_opt, end_opt) = if let Some(ref d) = duration {
        let secs = humantime::parse_duration(d)
            .map_err(|e| easy_error::err_msg(format!("invalid duration {d:?}: {e}")))?
            .as_secs();
        let end = SystemTime::now();
        let start = end - Duration::from_secs(secs);
        let si = info.shards_in_range(start, end)
            .map_err(|e| easy_error::err_msg(format!("catalog error: {e}")))?;
        (si, Some(start), Some(end))
    } else {
        let si = info.list_all()
            .map_err(|e| easy_error::err_msg(format!("catalog error: {e}")))?;
        (si, None, None)
    };

    let mut total = 0usize;
    for si in &shard_infos {
        let shard = cache.shard(si.start_time)
            .map_err(|e| easy_error::err_msg(format!("shard error: {e}")))?;
        let obs = shard.observability();

        let ids: Vec<Uuid> = if primary {
            match (start_opt, end_opt) {
                (Some(s), Some(e)) => obs.list_primaries_in_range(s, e),
                _ => obs.list_primaries(),
            }
        } else {
            // use UNIX_EPOCH sentinel as "all time" lower bound when no duration given
            let range_start = start_opt.unwrap_or(UNIX_EPOCH);
            let range_end   = end_opt.unwrap_or(si.end_time);
            obs.list_ids_by_time_range(range_start, range_end)
        }.map_err(|e| easy_error::err_msg(format!("query error: {e}")))?;

        for id in ids {
            if let Some(doc) = obs.get_by_id(id)
                .map_err(|e| easy_error::err_msg(format!("fetch error: {e}")))?
            {
                println!("{doc}");
                total += 1;
            }
        }
    }
    log::debug!("total: {total}");
    Ok(())
}
