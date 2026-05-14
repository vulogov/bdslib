use crossbeam::channel::{bounded, Receiver, Sender};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

/// How often the supervisor checks flusher liveness.
const SUPERVISOR_POLL: Duration = Duration::from_secs(5);

/// Process-wide flusher liveness, surfaced via `v2/status.ingest_flushers`.
///
/// A dead flusher used to be invisible — `Handle::stop` only `join`ed
/// the threads at shutdown, so a panicked flusher silently stopped
/// ingest until the next restart.  These counters + the supervisor
/// (see [`start`]) close that gap.
#[derive(Default)]
pub struct FlusherStats {
    /// Flusher threads currently running.  Maintained by [`AliveGuard`]
    /// — `Drop` runs on panic unwind too, so a panicked flusher
    /// correctly decrements before the supervisor respawns it.
    pub alive:          AtomicUsize,
    /// How many flushers *should* be running (`pipe_flushers`).
    pub configured:     AtomicUsize,
    /// Lifetime count of supervisor respawns.  Non-zero means a
    /// flusher panicked at least once — worth investigating the log
    /// for the `add_batch error` / panic that preceded it.
    pub restarts_total: AtomicU64,
    /// Lifetime count of records that could not be persisted.  A
    /// whole-batch insert failure triggers a per-record retry (see
    /// [`flush`]); only records that *also* fail individually count
    /// here.  Non-zero means genuine, identifiable data loss — the
    /// log has one `dropped poison record` line per increment.
    pub records_dropped: AtomicU64,
}

static STATS: OnceLock<FlusherStats> = OnceLock::new();

/// Borrow the process-wide flusher stats.  Lazily initialised.
pub fn stats() -> &'static FlusherStats {
    STATS.get_or_init(FlusherStats::default)
}

/// RAII liveness guard.  Increments `alive` on construction and
/// decrements on `Drop` — and `Drop` runs during panic unwinding, so
/// a flusher that panics out of [`run`] still leaves the count
/// accurate for the supervisor and `v2/status`.
struct AliveGuard;

impl AliveGuard {
    fn new() -> Self {
        stats().alive.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        stats().alive.fetch_sub(1, Ordering::Relaxed);
    }
}

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
/// Owns the **supervisor** thread, which in turn owns the flusher
/// threads.  Call [`Handle::stop`] on server shutdown to drain
/// remaining records and join everything before `sync_db`.
pub struct Handle {
    supervisor_shutdown: Option<Sender<()>>,
    supervisor:          Option<std::thread::JoinHandle<()>>,
}

impl Handle {
    /// Signal the supervisor to shut down — it then signals every
    /// flusher to drain the `"ingest"` channel and exit, joins them,
    /// and returns.  Blocks until the whole tree is down.
    pub fn stop(mut self) {
        if let Some(tx) = self.supervisor_shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.supervisor.take() {
            if let Err(e) = t.join() {
                log::error!("[add] supervisor thread panicked on shutdown: {e:?}");
            }
        }
    }
}

/// One running flusher: its dedicated shutdown channel + join handle.
/// The supervisor holds one of these per slot and replaces the whole
/// struct when it respawns a dead flusher.
struct FlusherSlot {
    shutdown_tx: Sender<()>,
    thread:      std::thread::JoinHandle<()>,
}

/// Spawn one flusher thread for `slot` and return its [`FlusherSlot`].
fn spawn_flusher(slot: usize, batch_size: usize, timeout: Duration) -> FlusherSlot {
    let (tx, rx) = bounded::<()>(1);
    let thread = std::thread::Builder::new()
        .name(format!("bds-add-{slot}"))
        .spawn(move || run(slot, batch_size, timeout, rx))
        .expect("failed to spawn bds-add thread");
    FlusherSlot { shutdown_tx: tx, thread }
}

/// Spawn the flusher **supervisor** and return a [`Handle`].
///
/// The supervisor spawns `cfg.flushers` flusher threads, then every
/// [`SUPERVISOR_POLL`] checks each one's `JoinHandle::is_finished()`.
/// A finished flusher means it exited unexpectedly (a normal exit
/// only happens on shutdown) — the supervisor logs it, respawns the
/// slot, and bumps `restarts_total`.  This is what stops a single
/// panic in the flush path from silently and permanently halting
/// ingest.
///
/// On [`Handle::stop`]: the supervisor signals every flusher to drain
/// its share of the `"ingest"` channel and exit, joins them, then
/// returns.
pub fn start(cfg: Config) -> Handle {
    let n_flushers = cfg.flushers.max(1);
    let timeout    = Duration::from_millis(cfg.timeout_ms);
    let batch_size = cfg.batch_size;
    stats().configured.store(n_flushers, Ordering::Relaxed);

    log::info!(
        "[add] supervisor starting {n_flushers} flusher thread{} \
         (batch_size={batch_size}, timeout={}ms, poll={}s)",
        if n_flushers == 1 { "" } else { "s" },
        cfg.timeout_ms,
        SUPERVISOR_POLL.as_secs(),
    );

    let (sup_tx, sup_rx) = bounded::<()>(1);
    let supervisor = std::thread::Builder::new()
        .name("bds-add-supervisor".to_string())
        .spawn(move || supervise(n_flushers, batch_size, timeout, sup_rx))
        .expect("failed to spawn bds-add supervisor thread");

    Handle {
        supervisor_shutdown: Some(sup_tx),
        supervisor:          Some(supervisor),
    }
}

/// Supervisor loop — owns the flusher slots, respawns dead ones,
/// drains+joins everything on shutdown.
fn supervise(
    n_flushers:  usize,
    batch_size:  usize,
    timeout:     Duration,
    shutdown_rx: Receiver<()>,
) {
    let mut slots: Vec<FlusherSlot> = (0..n_flushers)
        .map(|i| spawn_flusher(i, batch_size, timeout))
        .collect();

    loop {
        crossbeam::select! {
            recv(shutdown_rx) -> _ => {
                // Graceful shutdown: signal every flusher to drain +
                // exit, then join them all.
                for slot in &slots {
                    let _ = slot.shutdown_tx.send(());
                }
                for slot in slots.drain(..) {
                    if let Err(e) = slot.thread.join() {
                        log::error!("[add] flusher thread panicked on shutdown: {e:?}");
                    }
                }
                log::debug!("[add] supervisor stopped — all flushers joined");
                break;
            }
            default(SUPERVISOR_POLL) => {
                // Liveness sweep.  A flusher whose thread is finished
                // exited unexpectedly (panicked, or hit an
                // unrecoverable error) — a clean exit only happens via
                // the shutdown channel above.  Respawn the slot.
                for i in 0..slots.len() {
                    if slots[i].thread.is_finished() {
                        // Join the dead handle to surface its panic
                        // payload in the log, then replace the slot.
                        let dead = std::mem::replace(
                            &mut slots[i],
                            spawn_flusher(i, batch_size, timeout),
                        );
                        match dead.thread.join() {
                            Ok(())  => log::error!(
                                "[add] flusher {i} exited unexpectedly (no panic payload) \
                                 — respawned"
                            ),
                            Err(e)  => log::error!(
                                "[add] flusher {i} panicked: {e:?} — respawned"
                            ),
                        }
                        stats().restarts_total.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

fn run(worker_id: usize, batch_size: usize, timeout: Duration, shutdown_rx: Receiver<()>) {
    // Liveness guard — `Drop` runs on normal return AND on panic
    // unwind, so `stats().alive` is always accurate even when this
    // function panics out.  The supervisor uses `is_finished()`, not
    // this counter, to decide on respawns; `v2/status` reads it.
    let _alive = AliveGuard::new();

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
    bdslib::perf::time("ingest.flush", || flush_inner(worker_id, docs, n))
}

/// Persist one batch.  Fast path is a single `add_batch` call; on
/// failure it falls back to a **per-record retry** so one poison
/// record can't take down the whole batch (reliability finding #2 —
/// `execute_many` is all-or-nothing, so a single malformed record
/// used to drop up to `pipe_batch_size` good ones with it).
///
/// Returns the count of records successfully stored.
fn flush_inner(worker_id: usize, docs: Vec<serde_json::Value>, n: usize) -> usize {
    let db = match bdslib::get_db() {
        Ok(db) => db,
        Err(e) => {
            // No DB handle — nothing we can do with these records.
            log::error!("[add-{worker_id}] get_db failed — {n} records dropped: {e}");
            stats().records_dropped.fetch_add(n as u64, Ordering::Relaxed);
            return 0;
        }
    };

    // Retain a copy for the fallback path.  This clones the batch on
    // every flush — a real but bounded tax (a few ms for a 500-record
    // batch against a 30-80 ms flush) that buys guaranteed isolation
    // of poison records.  `add_batch` consumes its argument, so there
    // is no way to keep the originals without the clone.
    let retained = docs.clone();

    match db.add_batch(docs) {
        Ok(ids) => {
            log::debug!("[add-{worker_id}] flushed {n} records ({} stored)", ids.len());
            n
        }
        Err(e) => {
            log::warn!(
                "[add-{worker_id}] batch insert of {n} records failed ({e}); \
                 retrying per-record to isolate the failure"
            );
            let mut stored  = 0usize;
            let mut dropped  = 0u64;
            for doc in retained {
                // A 1-element batch goes through the identical
                // `ShardsManager::add_batch` path (same lock, same
                // engines) — but a failure now fails exactly one
                // record instead of the whole batch.
                match db.add_batch(vec![doc]) {
                    Ok(_)   => stored += 1,
                    Err(re) => {
                        dropped += 1;
                        log::error!("[add-{worker_id}] dropped poison record: {re}");
                    }
                }
            }
            if dropped > 0 {
                stats().records_dropped.fetch_add(dropped, Ordering::Relaxed);
            }
            log::warn!(
                "[add-{worker_id}] per-record fallback complete: {stored} stored, {dropped} dropped"
            );
            stored
        }
    }
}
