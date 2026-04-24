use std::time::Duration;

/// Configuration for the batch-ingestion thread.
///
/// Read from the hjson config file using the same keys as before:
///
/// | Key               | Type    | Default | Description |
/// |-------------------|---------|---------|-------------|
/// | `pipe_batch_size` | integer | 100     | Records per batch before flushing to the shard store. |
/// | `pipe_timeout_ms` | integer | 5000    | Milliseconds of channel inactivity before a partial batch is flushed. |
pub struct Config {
    pub batch_size: usize,
    pub timeout_ms: u64,
}

impl Config {
    /// Parse settings from the hjson config file.
    ///
    /// Returns `Ok(None)` only when no config path is available (neither
    /// `config_path` nor `BDS_CONFIG`).  Both fields default gracefully, so
    /// `Some` is returned whenever a config file can be located.
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
            .unwrap_or(100)
            .max(1);

        let timeout_ms = obj
            .get("pipe_timeout_ms")
            .and_then(|v| v.as_f64())
            .map(|n| n as u64)
            .unwrap_or(5000)
            .max(1);

        Ok(Some(Config { batch_size, timeout_ms }))
    }
}

/// Spawn the batch-ingestion thread.
///
/// Drains the `"ingest"` crossbeam channel, accumulates records into batches,
/// and calls [`ShardsManager::add_batch`] when either `batch_size` records are
/// queued or `timeout_ms` milliseconds pass with no new records.
///
/// Runs as a plain OS thread (not a tokio task) since DuckDB operations are
/// blocking and this thread spends most of its time in `recv_timeout`.
pub fn start(cfg: Config) -> std::thread::JoinHandle<()> {
    let timeout = Duration::from_millis(cfg.timeout_ms);
    std::thread::Builder::new()
        .name("bds-add".to_string())
        .spawn(move || run(cfg.batch_size, timeout))
        .expect("failed to spawn bds-add thread")
}

fn run(batch_size: usize, timeout: Duration) {
    eprintln!(
        "[add] started (batch_size={batch_size}, timeout={}ms)",
        timeout.as_millis()
    );
    let mut batch: Vec<serde_json::Value> = Vec::with_capacity(batch_size);

    loop {
        match bdslib::pipe::recv_timeout("ingest", timeout) {
            Ok(Some(doc)) => {
                batch.push(doc);
                if batch.len() >= batch_size {
                    flush(&mut batch);
                }
            }
            Ok(None) => {
                if !batch.is_empty() {
                    flush(&mut batch);
                }
            }
            Err(e) => {
                eprintln!("[add] channel error: {e}; thread exiting");
                if !batch.is_empty() {
                    flush(&mut batch);
                }
                break;
            }
        }
    }
}

fn flush(batch: &mut Vec<serde_json::Value>) {
    let docs = std::mem::take(batch);
    let n = docs.len();
    match bdslib::get_db().and_then(|db| db.add_batch(docs)) {
        Ok(ids) => eprintln!("[add] flushed {n} records ({} stored)", ids.len()),
        Err(e) => eprintln!("[add] add_batch error: {e}"),
    }
}
