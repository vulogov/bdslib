use crossbeam::channel::{bounded, Receiver, Sender};
use std::time::Duration;

/// Configuration for the batch-ingestion threads.
///
/// | Key                | Type    | Default | Description |
/// |--------------------|---------|---------|-------------|
/// | `pipe_batch_size`  | integer | 500     | Records per batch before flushing to the shard store. |
/// | `pipe_timeout_ms`  | integer | 500     | Milliseconds of channel inactivity before a partial batch is flushed. |
/// | `pipe_flushers`    | integer | 1       | Number of concurrent flusher threads draining the ingest channel.  Clamped to `[1, 16]`. |
///
/// The defaults trade interactive latency (~500ms worst case for a
/// trickle of records) for throughput (large batches amortise the
/// Tantivy-commit / DuckDB-transaction / ONNX-batch costs).  Lower
/// `pipe_timeout_ms` for a more interactive feel; raise
/// `pipe_batch_size` if your workload is consistently dense.
///
/// `pipe_flushers` spawns N flusher threads that share the same
/// `"ingest"` crossbeam MPMC channel.  **The flush operation itself is
/// internally serialised by `ShardsManager::add_batch`** because
/// DuckDB, Tantivy, and HNSW are not safe under concurrent same-shard
/// writers.  Extra flushers therefore don't speed up flushing — they
/// only let one thread accumulate a new batch while another is mid-
/// flush.  Default is `1`; raise only if your workload is bursty
/// enough that batch accumulation is the bottleneck (rare).
pub struct Config {
    pub batch_size: usize,
    pub timeout_ms: u64,
    pub flushers:   usize,
}

impl Config {
    /// Parse settings from the hjson config file.
    ///
    /// Returns `Ok(None)` only when no config path is available.
    pub fn from_config(config_path: Option<&str>) -> anyhow::Result<Option<Self>> {
        let path = match config_path {
            Some(p) => p.to_string(),
            None => match std::env::var("BDS_CONFIG") {
                Ok(p) => p,
                Err(_) => return Ok(None),
            },
        };

        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("cannot read config {path:?}: {e}"))?;
        let val: serde_hjson::Value = serde_hjson::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("hjson parse error in {path:?}: {e}"))?;
        let obj = val
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("config must be a JSON object"))?;

        let batch_size = obj
            .get("pipe_batch_size")
            .and_then(|v| v.as_f64())
            .map(|n| n as usize)
            .unwrap_or(500)
            .max(1);

        let timeout_ms = obj
            .get("pipe_timeout_ms")
            .and_then(|v| v.as_f64())
            .map(|n| n as u64)
            .unwrap_or(500)
            .max(1);

        let flushers = obj
            .get("pipe_flushers")
            .and_then(|v| v.as_f64())
            .map(|n| n as usize)
            .unwrap_or(1)
            .clamp(1, 16);

        Ok(Some(Config { batch_size, timeout_ms, flushers }))
    }
}

/// Handle returned by [`start`].
///
/// Call [`Handle::stop`] on server shutdown to drain remaining records from
/// the `"ingest"` channel and join all flusher threads before calling
/// `sync_db`.
pub struct Handle {
    /// One shutdown sender per spawned flusher thread.  Each thread
    /// owns a distinct receiver (crossbeam channels don't broadcast),
    /// so we signal them one-by-one on `stop()`.
    shutdown_txs: Vec<Sender<()>>,
    threads:      Vec<std::thread::JoinHandle<()>>,
}

impl Handle {
    /// Signal every flusher to drain the `"ingest"` channel and exit,
    /// then block until they all finish.
    pub fn stop(mut self) {
        for tx in self.shutdown_txs.drain(..) {
            let _ = tx.send(());
        }
        for t in self.threads.drain(..) {
            if let Err(e) = t.join() {
                log::error!("[add] flusher thread panicked on shutdown: {e:?}");
            }
        }
    }
}

/// Spawn `cfg.flushers` batch-ingestion threads and return a [`Handle`]
/// for graceful shutdown.
///
/// Each thread drains the same `"ingest"` crossbeam MPMC channel,
/// accumulates records into batches, and calls
/// [`ShardsManager::add_batch`] when either `batch_size` records are
/// queued or `timeout_ms` milliseconds pass with no new records.
///
/// On [`Handle::stop`]: every thread drains its share of records still
/// in the channel before exiting, ensuring no data is lost.
pub fn start(cfg: Config) -> Handle {
    let n_flushers = cfg.flushers.max(1);
    let timeout    = Duration::from_millis(cfg.timeout_ms);
    log::info!(
        "[add] spawning {n_flushers} flusher thread{} (batch_size={}, timeout={}ms)",
        if n_flushers == 1 { "" } else { "s" },
        cfg.batch_size,
        cfg.timeout_ms,
    );

    let mut shutdown_txs = Vec::with_capacity(n_flushers);
    let mut threads      = Vec::with_capacity(n_flushers);
    for i in 0..n_flushers {
        let (tx, rx) = bounded::<()>(1);
        let batch_size = cfg.batch_size;
        let t = std::thread::Builder::new()
            .name(format!("bds-add-{i}"))
            .spawn(move || run(i, batch_size, timeout, rx))
            .expect("failed to spawn bds-add thread");
        shutdown_txs.push(tx);
        threads.push(t);
    }
    Handle { shutdown_txs, threads }
}

fn run(worker_id: usize, batch_size: usize, timeout: Duration, shutdown_rx: Receiver<()>) {
    log::debug!(
        "[add-{worker_id}] started (batch_size={batch_size}, timeout={}ms)",
        timeout.as_millis()
    );

    let ingest_rx = match bdslib::pipe::receiver("ingest") {
        Ok(r) => r,
        Err(e) => {
            log::error!("[add-{worker_id}] cannot access ingest channel: {e}");
            return;
        }
    };

    let mut batch: Vec<serde_json::Value> = Vec::with_capacity(batch_size);
    let mut total_records: u64 = 0;
    let run_start = std::time::Instant::now();
    let mut batch_start = std::time::Instant::now();
    // Wall-clock at which the FIRST doc of the current batch arrived.
    // Reset on every flush; used to record `ingest.lag` — the time the
    // oldest queued doc waited before being persisted.  Distinct from
    // `batch_start`, which is reset on flush/timeout regardless of
    // whether a new doc arrived afterwards.
    let mut first_doc_time: Option<std::time::Instant> = None;

    loop {
        crossbeam::select! {
            recv(ingest_rx) -> msg => {
                match msg {
                    Ok(doc) => {
                        if batch.is_empty() {
                            first_doc_time = Some(std::time::Instant::now());
                        }
                        batch.push(doc);
                        if batch.len() >= batch_size {
                            record_lag(&mut first_doc_time);
                            let n = flush(worker_id, &mut batch) as u64;
                            total_records += n;
                            let batch_secs = batch_start.elapsed().as_secs_f64();
                            batch_start = std::time::Instant::now();
                            if batch_secs > 0.0 {
                                log::debug!(
                                    "[add-{worker_id}] throughput: {:.1} records/s ({total_records} total)",
                                    n as f64 / batch_secs
                                );
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            recv(shutdown_rx) -> _ => {
                // Drain every record still sitting in the channel before exiting.
                while let Ok(doc) = ingest_rx.try_recv() {
                    if batch.is_empty() {
                        first_doc_time = Some(std::time::Instant::now());
                    }
                    batch.push(doc);
                    if batch.len() >= batch_size {
                        record_lag(&mut first_doc_time);
                        total_records += flush(worker_id, &mut batch) as u64;
                    }
                }
                if !batch.is_empty() {
                    record_lag(&mut first_doc_time);
                    total_records += flush(worker_id, &mut batch) as u64;
                }
                let elapsed = run_start.elapsed().as_secs_f64();
                if elapsed > 0.0 {
                    log::debug!(
                        "[add-{worker_id}] shutdown complete — {total_records} records in {elapsed:.1}s ({:.1} avg records/s)",
                        total_records as f64 / elapsed
                    );
                } else {
                    log::debug!("[add-{worker_id}] shutdown complete — {total_records} records");
                }
                break;
            }
            default(timeout) => {
                if !batch.is_empty() {
                    record_lag(&mut first_doc_time);
                    total_records += flush(worker_id, &mut batch) as u64;
                    batch_start = std::time::Instant::now();
                }
            }
        }
    }
}

/// Record ingest.lag (µs since first doc of the current batch arrived)
/// and clear the timer.  Called immediately before every flush.
fn record_lag(first_doc_time: &mut Option<std::time::Instant>) {
    if let Some(t) = first_doc_time.take() {
        bdslib::perf::record_us("ingest.lag", t.elapsed().as_micros() as u64);
    }
}

fn flush(worker_id: usize, batch: &mut Vec<serde_json::Value>) -> usize {
    let docs = std::mem::take(batch);
    let n = docs.len();
    bdslib::perf::record("ingest.batch_size", n as u64);
    bdslib::perf::time("ingest.flush", || {
        match bdslib::get_db().and_then(|db| db.add_batch(docs)) {
            Ok(ids) => {
                log::debug!("[add-{worker_id}] flushed {n} records ({} stored)", ids.len());
                n
            }
            Err(e) => {
                log::error!("[add-{worker_id}] add_batch error: {e}");
                0
            }
        }
    })
}
