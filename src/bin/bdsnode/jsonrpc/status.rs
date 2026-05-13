use jsonrpsee::RpcModule;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/status", |_params, _ctx, _| async move {
            log::debug!("v2/status: start");

            let state = crate::status::get();

            let uptime_secs     = state.started_at.elapsed().as_secs();
            let timestamp       = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let logs_queue         = bdslib::pipe::len("ingest").unwrap_or(0);
            let json_file_queue    = bdslib::pipe::len("ingest_file").unwrap_or(0);
            let syslog_file_queue  = bdslib::pipe::len("ingest_file_syslog").unwrap_or(0);
            let json_file_name     = state.current_file
                .lock()
                .ok()
                .and_then(|g| g.clone());
            let syslog_file_name   = state.current_syslog_file
                .lock()
                .ok()
                .and_then(|g| g.clone());

            let (jsoncache_pct, jsoncache_len, jsoncache_capacity, embedding_model) =
                match bdslib::get_db() {
                    Ok(db) => (
                        db.jsoncache_utilization_pct(),
                        db.jsoncache_len() as u64,
                        db.jsoncache_capacity() as u64,
                        db.embedding_model_name(),
                    ),
                    Err(_) => (0, 0, 0, None),
                };

            // Cluster summary — compact, suitable for dashboard tile.
            let cluster_info = bdslib::get_db().ok().and_then(|db| {
                db.cluster().map(|c| {
                    let (alive, suspect, dead) = c.peers.read().count_by_state();
                    let hint_backlog = c.hints.len().unwrap_or(0);
                    serde_json::json!({
                        "node_id":  c.node_id.to_string(),
                        "bind_url": c.config.bind_url,
                        "mode":     c.mode().as_str(),
                        "alive":    alive,
                        "suspect":  suspect,
                        "dead":     dead,
                        "full_mode_threshold": c.config.full_mode_threshold,
                        "replication_factor":  c.config.replication_factor,
                        "hint_backlog":        hint_backlog,
                    })
                })
            });

            // BUND runtime stats (BundWorkerPool + result queues + named contexts).
            let n_results = bdslib::vm::results().n_queues() as u64;
            let n_bunds   = bdslib::vm::context::n_contexts() as u64;

            let recent_scripts: Vec<serde_json::Value> = bdslib::vm::workers::recent_submissions()
                .into_iter()
                .map(|(id, ts)| serde_json::json!({
                    "id":           id.to_string(),
                    "submitted_at": ts,
                }))
                .collect();

            let running_scripts: Vec<serde_json::Value> = bdslib::vm::workers::running_snapshot()
                .into_iter()
                .map(|(worker_id, job_id)| serde_json::json!({
                    "worker": worker_id,
                    "id":     job_id.to_string(),
                }))
                .collect();

            // Retention sweeper stats — always present so operators
            // can tell at a glance whether retention is running or
            // turned off.  Values are atomic counters maintained by
            // `bdslib::retention::record_run` after every sweep.
            let retention_block = {
                use std::sync::atomic::Ordering;
                let s = bdslib::retention::stats();
                serde_json::json!({
                    "evicted_lifetime":        s.evicted_lifetime.load(Ordering::Relaxed),
                    "evicted_last_run":        s.evicted_last_run.load(Ordering::Relaxed),
                    "freed_lifetime_bytes":    s.freed_lifetime_bytes.load(Ordering::Relaxed),
                    "freed_last_run_bytes":    s.freed_last_run_bytes.load(Ordering::Relaxed),
                    "last_run_ts":             s.last_run_ts.load(Ordering::Relaxed),
                    "last_run_ms":             s.last_run_ms.load(Ordering::Relaxed),
                    "errors_lifetime":         s.errors_lifetime.load(Ordering::Relaxed),
                    "quorum_skipped_lifetime": s.quorum_skipped_lifetime.load(Ordering::Relaxed),
                    "quorum_skipped_last_run": s.quorum_skipped_last_run.load(Ordering::Relaxed),
                })
            };

            let value = serde_json::json!({
                "node_id":           state.node_id,
                "hostname":          state.hostname,
                "version":           env!("CARGO_PKG_VERSION"),
                "uptime_secs":       uptime_secs,
                "timestamp":         timestamp,
                "logs_queue":        logs_queue,
                "json_file_queue":   json_file_queue,
                "json_file_name":    json_file_name,
                "syslog_file_queue": syslog_file_queue,
                "syslog_file_name":  syslog_file_name,
                "jsoncache_pct":      jsoncache_pct,
                "jsoncache_len":      jsoncache_len,
                "jsoncache_capacity": jsoncache_capacity,
                "embedding_model":    embedding_model,
                "n_results":          n_results,
                "n_bunds":            n_bunds,
                "recent_scripts":     recent_scripts,
                "running_scripts":    running_scripts,
                "cluster":            cluster_info,
                "retention":          retention_block,
            });

            log::debug!("v2/status: done");
            Ok::<serde_json::Value, jsonrpsee::types::ErrorObject>(value)
        })
        .unwrap();
}
