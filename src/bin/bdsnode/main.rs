mod jsonrpc;
mod server;
mod status;

use anyhow::Context;
use bdslib::vm::workers::BundWorkerPool;
use clap::Parser;
use jsonrpsee::server::Server;
use std::sync::OnceLock;

/// Process-wide BUND worker pool.  Initialised in `main()` before the
/// JSON-RPC server starts.  Workers run for the lifetime of the process.
static WORKERS: OnceLock<BundWorkerPool> = OnceLock::new();

fn nofile_limit_from_config(config_path: Option<&str>) -> u64 {
    const DEFAULT: u64 = 4096;
    let path = match config_path {
        Some(p) => p,
        None => return DEFAULT,
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return DEFAULT,
    };
    let val: serde_hjson::Value = match serde_hjson::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return DEFAULT,
    };
    val.as_object()
       .and_then(|o| o.get("nofile_limit"))
       .and_then(|v| v.as_f64())
       .map(|n| n as u64)
       .unwrap_or(DEFAULT)
}

fn n_workers_from_config(config_path: Option<&str>) -> usize {
    const DEFAULT: usize = 4;
    let path = match config_path {
        Some(p) => p,
        None => return DEFAULT,
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return DEFAULT,
    };
    let val: serde_hjson::Value = match serde_hjson::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return DEFAULT,
    };
    val.as_object()
       .and_then(|o| o.get("n_workers"))
       .and_then(|v| v.as_f64())
       .map(|n| (n as usize).max(1))
       .unwrap_or(DEFAULT)
}

/// Capacity for the ingest channels (`ingest`, `ingest_file`,
/// `ingest_file_syslog`).
///
/// Returns the `ingest_channel_capacity` config value, or `100_000` if
/// unset.  `0` means "unbounded" (the legacy behaviour, susceptible to
/// OOM under producer pressure).
fn ingest_channel_capacity_from_config(config_path: Option<&str>) -> usize {
    const DEFAULT: usize = 100_000;
    let path = match config_path {
        Some(p) => p,
        None => return DEFAULT,
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return DEFAULT,
    };
    let val: serde_hjson::Value = match serde_hjson::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return DEFAULT,
    };
    val.as_object()
       .and_then(|o| o.get("ingest_channel_capacity"))
       .and_then(|v| v.as_f64())
       .map(|n| n as usize)
       .unwrap_or(DEFAULT)
}

/// Read `perf.slow_query_threshold_ms` from hjson and install it into
/// the perf module.  Missing block / missing key / invalid value all
/// keep the bdslib default (500 ms).  Set the key to `0` to disable
/// the slow log entirely.
fn apply_slow_query_threshold(config_path: Option<&str>) {
    let Some(path) = config_path else { return };
    let Ok(raw) = std::fs::read_to_string(path) else { return };
    let Ok(val) = serde_hjson::from_str::<serde_hjson::Value>(&raw) else { return };
    let Some(ms) = val.as_object()
        .and_then(|o| o.get("perf"))
        .and_then(|p| p.as_object())
        .and_then(|p| p.get("slow_query_threshold_ms"))
        .and_then(|v| v.as_f64())
    else { return };
    let us = (ms.max(0.0) as u64).saturating_mul(1000);
    bdslib::perf::set_slow_threshold_us(us);
    log::info!("[perf] slow-query threshold = {ms} ms ({us} µs)");
}

fn raise_nofile_limit(limit: u64) {
    match rlimit::increase_nofile_limit(limit) {
        Ok(n)  => log::info!("NOFILE soft limit raised to {n}"),
        Err(e) => log::warn!("could not raise NOFILE limit: {e}"),
    }
}

#[derive(Parser)]
#[command(name = "bdsnode", about = "BDS JSON-RPC 2.0 server")]
struct Cli {
    /// Path to the hjson configuration file (overrides BDS_CONFIG env var).
    #[arg(short, long, env = "BDS_CONFIG")]
    config: Option<String>,

    /// Address to bind the JSON-RPC listener.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Port for the JSON-RPC listener.
    #[arg(short, long, default_value_t = 9000)]
    port: u16,

    /// Log verbosity (0=env default, 1=info, 2=debug, 3=trace).
    #[arg(short = 'd', long, default_value_t = 0)]
    debug: u32,

    /// Node identifier included in v2/status responses.
    ///
    /// Pass an explicit value for fixed cluster identities (e.g. a hostname or
    /// role name).  When omitted a UUID v7 is generated at startup.
    #[arg(long)]
    nodeid: Option<String>,

    /// Wipe the existing data store and start fresh before opening.
    ///
    /// Reads `dbpath` from the config file, removes the directory tree, then
    /// proceeds with normal initialisation. Use with care — all data is lost.
    #[arg(long, default_value_t = false)]
    new: bool,

    /// **Dev/demo only.**  Enable the synthetic-data generator
    /// regardless of `generate_realistic_data.enabled` in bds.hjson.
    /// The node emits a loud startup banner and every status surface
    /// flags the data as artificial.  Other knobs (interval, total,
    /// scenarios, ratios) still come from the hjson
    /// `generate_realistic_data:` block.
    #[arg(long = "generate_realistic_data", default_value_t = false)]
    generate_realistic_data: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let node_id = cli.nodeid.clone()
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    status::init(node_id);

    bdslib::setloglevel::setloglevel(cli.debug);
    raise_nofile_limit(nofile_limit_from_config(cli.config.as_deref()));

    if cli.new {
        let dbpath = bdslib::dbpath_from_config(cli.config.as_deref())
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("failed to read dbpath from config for --new")?;
        if std::path::Path::new(&dbpath).exists() {
            std::fs::remove_dir_all(&dbpath)
                .with_context(|| format!("--new: failed to remove {dbpath}"))?;
            log::info!("--new: removed existing data store at {dbpath}");
        } else {
            log::info!("--new: {dbpath} does not exist, nothing to remove");
        }
    }

    bdslib::init_db(cli.config.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to initialise database")?;

    // Apply the slow-query threshold to the perf module.  Keeps the
    // bdslib API surface tiny — one OnceLock-backed setter — and
    // means every existing `perf::time` call participates without
    // per-site instrumentation.  Default kicks in (500 ms) when no
    // hjson key is present.
    apply_slow_query_threshold(cli.config.as_deref());

    jsonrpc::chat_ollama::init(cli.config.as_deref())
        .context("failed to initialise Ollama config")?;

    // Phase 0 of v4/* LLM surface — register provider clients from the
    // `llm` block of bds.hjson.  Empty manager (no providers) is fine
    // and just means no v4/* RPC will resolve until config is added.
    let llm_cfg = match cli.config.as_deref() {
        Some(path) => bdslib::llm::LlmConfig::load_from_hjson(path),
        None       => bdslib::llm::LlmConfig::default(),
    };
    {
        let mgr = bdslib::llm::ProviderManager::from_config(llm_cfg.clone());
        if mgr.is_empty() {
            log::info!("[llm] no providers registered (missing `llm` block or all skipped)");
        } else {
            log::info!("[llm] {} provider(s) registered: {:?} (default={:?})",
                mgr.len(), mgr.registered(), mgr.default_id());
        }
        bdslib::llm::manager::init(mgr);
    }

    // Phase 3 — inference cache.  Opens <dbpath>/llm/cache.duckdb when
    // llm.cache.enabled is true (default) and a config supplied a
    // dbpath; standalone runs without config fall through to "cache
    // disabled" and every helper call goes straight to the provider.
    {
        let cache_enabled = llm_cfg.cache.enabled;
        let ttl_secs      = llm_cfg.cache.ttl_secs;
        let dbpath = cli.config.as_deref().and_then(|p|
            bdslib::dbpath_from_config(Some(p)).ok()
        );
        if !cache_enabled {
            log::info!("[llm] cache disabled (llm.cache.enabled=false)");
        } else if let Some(dbpath) = dbpath {
            let cache_root = std::path::Path::new(&dbpath).join("llm");
            match bdslib::llm::cache::InferenceCache::open(&cache_root) {
                Ok(cache) => {
                    let rows = cache.count().unwrap_or(0);
                    log::info!("[llm] cache opened at {} (rows={rows}, ttl={ttl_secs}s)",
                        cache_root.display());
                    bdslib::llm::cache::init(
                        bdslib::llm::cache::CacheManager::new(cache, true, ttl_secs),
                    );
                }
                Err(e) => log::warn!("[llm] cache open failed at {}: {e}", cache_root.display()),
            }
        } else {
            log::info!("[llm] cache requested but no dbpath available; cache will stay unset");
        }
    }

    // Phase 5 — async / offline LLM job queue.  Opens
    // <dbpath>/llm/jobs.duckdb when a dbpath is available; standalone
    // runs without config skip it.  The runner that consumes pending
    // jobs is spawned later (phase 5.c wires this into the existing
    // background-task scaffolding).
    {
        let dbpath = cli.config.as_deref().and_then(|p|
            bdslib::dbpath_from_config(Some(p)).ok()
        );
        if let Some(dbpath) = dbpath {
            let jobs_root = std::path::Path::new(&dbpath).join("llm");
            match bdslib::llm::jobs::JobQueue::open(&jobs_root) {
                Ok(q) => {
                    let pending = q.count_in_state(bdslib::llm::jobs::JobState::Pending)
                        .unwrap_or(0);
                    log::info!("[llm] job queue opened at {} (pending={pending})",
                        jobs_root.display());
                    bdslib::llm::jobs::init(q);
                }
                Err(e) => log::warn!("[llm] job queue open failed at {}: {e}",
                    jobs_root.display()),
            }
        }
    }

    // Phase 4 — cluster-wide inference dedup runtime settings.  The
    // per-node InferenceLog lives on Cluster (init_db opens it inside
    // Cluster::init when cluster mode is on); these are the runtime
    // toggles the helpers consult to decide whether to apply dedup
    // and how long to wait for a peer's running inference.
    bdslib::llm::dedup::init_settings(bdslib::llm::dedup::DedupSettings {
        enabled:       llm_cfg.dedup.enabled,
        window_secs:   llm_cfg.dedup.window_secs,
        wait_max_secs: llm_cfg.dedup.wait_max_secs,
    });
    log::info!("[llm] dedup: enabled={} window={}s wait_max={}s",
        llm_cfg.dedup.enabled, llm_cfg.dedup.window_secs, llm_cfg.dedup.wait_max_secs);

    // Chat-snippet eval (llm.chat.bund.*) — DEFAULTS OFF.  Enabling
    // grants chat users the same code-execution privileges as a
    // stored scheduled BUND script.  See Documentation/LLM.md.
    {
        let b = &llm_cfg.chat.bund;
        bdslib::llm::chat_bund::init_settings(bdslib::llm::chat_bund::ChatBundSettings {
            enabled:           b.enabled,
            timeout_secs:      b.timeout_secs,
            max_result_chars:  b.max_result_chars,
            oversize_strategy: bdslib::llm::chat_bund::OversizeStrategy::from_wire(&b.oversize_strategy),
            slash_strictness:  bdslib::llm::snippet::SlashStrictness::from_wire(&b.slash_strictness),
            fenced_only:       b.fenced_only,
        });
        if b.enabled {
            log::warn!("[llm] chat.bund: ENABLED — chat users can execute arbitrary BUND \
                        (timeout={}s, max_result_chars={}, strategy={}, slash={}, fenced_only={})",
                       b.timeout_secs, b.max_result_chars,
                       b.oversize_strategy, b.slash_strictness, b.fenced_only);
        } else {
            log::info!("[llm] chat.bund: disabled (default).  Detected snippets fall through \
                        to the standard aggregation RAG path.");
        }
    }

    // English → Bund translator (llm.to_bund.*) — defaults ON.  The
    // RPC itself does NOT execute the generated script; consumers
    // (chat, bdscmd, bdsweb) decide whether to run it.  See
    // src/llm/to_bund.rs.
    {
        let t = &llm_cfg.to_bund;
        bdslib::llm::to_bund::init_settings(bdslib::llm::to_bund::ToBundSettings {
            enabled:             t.enabled,
            timeout_secs:        t.timeout_secs,
            max_retries:         t.max_retries,
            provider:            t.provider.clone(),
            model:               t.model.clone(),
            extra_system_prompt: t.extra_system_prompt.clone(),
        });
        if t.enabled {
            log::info!("[llm] to_bund: enabled (timeout={}s, max_retries={}, provider={:?}, model={:?}, extra_prompt_len={})",
                t.timeout_secs, t.max_retries,
                if t.provider.is_empty() { "<default>" } else { t.provider.as_str() },
                if t.model.is_empty()    { "<provider-default>" } else { t.model.as_str() },
                t.extra_system_prompt.len());
        } else {
            log::warn!("[llm] to_bund: disabled (llm.to_bund.enabled = false) — v2/to.bund will return errors");
        }
    }

    // BUND sandbox — gates dangerous words (shell, FS write, cluster
    // admin, etc.) per the `bund.disabled_categories` /
    // `bund.disabled_words` block in bds.hjson.  Defaults to no
    // denials (every category enabled) for backwards compatibility.
    // MUST run before `init_adam`: the policy is applied each time a
    // VM is initialised (Adam, ephemeral, workers), so the OnceLock
    // has to be populated first or the very first VM ships
    // unsandboxed.
    {
        let pol = match cli.config.as_deref() {
            Some(path) => bdslib::bund_policy::Policy::load_from_hjson(path),
            None       => bdslib::bund_policy::Policy::default(),
        };
        bdslib::bund_policy::init_policy(pol);
        bdslib::bund_policy::log_policy_summary();
    }

    bdslib::init_adam()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to initialise BUND VM")?;

    let n_workers = n_workers_from_config(cli.config.as_deref());
    let pool = BundWorkerPool::start(n_workers)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to initialise BundWorkerPool")?;
    WORKERS.set(pool).ok();
    log::info!("BundWorkerPool started with {n_workers} worker(s)");

    bdslib::context::init(cli.config.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to initialise BUND context")?;

    // Bound the ingest channels so a producer flood (or a stalled
    // consumer thread) returns "full" to the RPC layer instead of
    // OOMing the process by silently growing an unbounded queue.
    // `0` means unbounded (back-compat for callers that need it).
    let ingest_capacity = ingest_channel_capacity_from_config(cli.config.as_deref());
    bdslib::pipe::init_with_capacity(&[
        ("ingest",              ingest_capacity),
        ("ingest_file",         ingest_capacity),
        ("ingest_file_syslog",  ingest_capacity),
    ])
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to initialise pipe registry")?;

    let cleanup_cfg = server::bundcleanup::Config::from_config(cli.config.as_deref())
        .context("failed to read BUND cleanup config")?;
    let cleanup_handle = server::bundcleanup::start(cleanup_cfg);

    let results_cfg = server::results_sweeper::Config::from_config(cli.config.as_deref())
        .context("failed to read result-queue sweeper config")?;
    let results_sweeper_handle = server::results_sweeper::start(results_cfg);

    // Cron-driven script scheduler — fires stored BUND scripts whose
    // `schedule` metadata matches the current minute. Disabled by setting
    // `scheduler_interval_secs: 0` in bds.hjson.
    let scheduler_cfg = server::scheduler::Config::from_config(cli.config.as_deref())
        .context("failed to read scheduler config")?;
    let scheduler_handle = server::scheduler::start(scheduler_cfg);

    // Phase 5.c — async LLM job runner.  Drains <dbpath>/llm/jobs.duckdb,
    // pushes results onto the per-node ResultQueue under each job's
    // result_id, marks rows terminal.  No-op when the queue isn't
    // initialised or when llm.runner.enabled is false.
    let llm_runner_cfg = server::llm_jobs::Config::from_config(cli.config.as_deref())
        .context("failed to read llm.runner config")?;
    let llm_runner_handle = server::llm_jobs::start(llm_runner_cfg);

    // Periodic global sync — checkpoints DuckDB WAL, commits Tantivy, flushes
    // VecStore on every open shard. Bounds recovery time after an unclean
    // exit. Disabled by setting `sync_interval_secs: 0` in bds.hjson.
    let sync_cfg = server::sync::Config::from_config(cli.config.as_deref())
        .context("failed to read sync config")?;
    let sync_handle = server::sync::start(sync_cfg);

    // Phase 1 shard retention — sweeper that drops shards whose end_ts
    // is older than `retention.duration`.  Off by default (opt-in
    // feature).  Crash-recovery for half-completed evictions runs once
    // here, before the background task spawns and before the JSON-RPC
    // listener binds (so a racing ingest can't pick up an `evicting=true`
    // catalog row).
    if let Ok(db) = bdslib::get_db() {
        match db.cleanup_orphan_evicting() {
            Ok(0) => {}
            Ok(n) => log::info!("[retention] startup: cleaned {n} orphan shard(s) from a previous crashed sweep"),
            Err(e) => log::warn!("[retention] startup orphan cleanup failed: {e}"),
        }
    }
    let retention_cfg = server::retention::Config::from_config(cli.config.as_deref())
        .context("failed to read retention config")?;
    let retention_handle = server::retention::start(retention_cfg);

    // Data rebalancer — opt-in background task that scans sharded
    // telemetry for under-replicated records and pushes them to
    // peers that don't have them.  No-op when `rebalancer.enabled =
    // false` (the default) or when the node is standalone.
    let rebalancer_cfg = bdslib::rebalancer::RebalancerConfig::from_path(
        cli.config.as_deref().unwrap_or(""))
        .unwrap_or_else(|e| {
            log::warn!("[rebalancer] config parse failed ({e}); disabled");
            bdslib::rebalancer::RebalancerConfig::disabled()
        });
    let rebalancer_handle = server::rebalancer::start(rebalancer_cfg);

    // Cluster gossip task — bootstraps against `cluster.bootstrap` (if set)
    // then runs a periodic ping/peers exchange + liveness sweep.  No-op when
    // `cluster.enabled = false` in bds.hjson.
    let cluster_handle = server::cluster::start();

    // Dev/demo synthetic-data generator — gated behind hjson
    // `generate_realistic_data.enabled` OR the `--generate_realistic_data`
    // CLI flag.  When armed, emits a loud multi-line banner via
    // log::warn so operators can't mistake a demo node for prod, and
    // pushes one batch of fake telemetry through the `ingest` pipe
    // every `interval_secs` seconds.  No-op otherwise.
    let dev_data_cfg = server::dev_data::Config::from_config(
        cli.config.as_deref(),
        cli.generate_realistic_data,
    ).context("failed to read generate_realistic_data config")?;
    let dev_data_handle = server::dev_data::start(dev_data_cfg);

    let add_handle = if let Some(cfg) = server::add::Config::from_config(cli.config.as_deref())
        .context("failed to read ingest config")?
    {
        Some(server::add::start(cfg))
    } else {
        None
    };

    let add_file_handle =
        if let Some(cfg) = server::add_file::Config::from_config(cli.config.as_deref())
            .context("failed to read file-ingest config")?
        {
            Some(server::add_file::start(cfg, status::get().current_file.clone()))
        } else {
            None
        };

    let add_file_syslog_handle =
        if let Some(cfg) = server::add_file_syslog::Config::from_config(cli.config.as_deref())
            .context("failed to read syslog file-ingest config")?
        {
            Some(server::add_file_syslog::start(cfg, status::get().current_syslog_file.clone()))
        } else {
            None
        };

    let addr = format!("{}:{}", cli.host, cli.port);

    let server = Server::builder()
        .build(&addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    let local_addr = server.local_addr()?;
    let handle = server.start(jsonrpc::build_module());

    log::info!("bdsnode listening on {local_addr}");

    tokio::signal::ctrl_c().await.context("ctrl-c signal error")?;

    log::info!("shutting down…");
    handle.stop()?;
    handle.stopped().await;

    cleanup_handle.stop().await;
    server::bundcleanup::vm_close();

    results_sweeper_handle.stop().await;
    scheduler_handle.stop().await;
    llm_runner_handle.stop().await;
    sync_handle.stop().await;
    rebalancer_handle.stop().await;
    retention_handle.stop().await;
    dev_data_handle.stop().await;
    cluster_handle.stop().await;

    // Drain ingest channels and join batch threads before checkpointing so
    // that no queued records are lost.
    if let Some(h) = add_file_syslog_handle {
        h.stop();
    }
    if let Some(h) = add_file_handle {
        h.stop();
    }
    if let Some(h) = add_handle {
        h.stop();
    }

    bdslib::sync_db().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
