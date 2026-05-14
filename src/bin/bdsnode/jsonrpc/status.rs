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

            // Dev/demo synthetic-data generator state.  Always
            // emitted — callers branch on `enabled` to render the
            // "SYNTHETIC DATA" warning banner.  Atomic loads, no
            // lock contention with the live generator.
            let dev_data_block = {
                use std::sync::atomic::Ordering;
                let s = bdslib::dev_data::stats();
                let mut o = serde_json::json!({
                    "enabled":            s.enabled.load(Ordering::Relaxed),
                    "records_lifetime":   s.records_lifetime.load(Ordering::Relaxed),
                    "records_last_batch": s.records_last_batch.load(Ordering::Relaxed),
                    "batches_emitted":    s.batches_emitted.load(Ordering::Relaxed),
                    "last_run_ts":        s.last_run_ts.load(Ordering::Relaxed),
                    "last_run_ms":        s.last_run_ms.load(Ordering::Relaxed),
                    "errors_lifetime":    s.errors_lifetime.load(Ordering::Relaxed),
                });
                // When the active config is installed echo the knobs
                // so dashboards can render "Generating N records every
                // Ms covering D" without re-reading hjson.
                if let Some(a) = crate::server::dev_data::active() {
                    if let Some(obj) = o.as_object_mut() {
                        obj.insert("config_enabled".to_owned(),  serde_json::json!(a.enabled));
                        obj.insert("interval_secs".to_owned(),   serde_json::json!(a.interval_secs));
                        obj.insert("duration".to_owned(),        serde_json::json!(a.duration));
                        obj.insert("total_per_batch".to_owned(), serde_json::json!(a.total));
                        obj.insert("scenarios".to_owned(),       serde_json::json!(a.scenarios));
                        obj.insert("noise_ratio".to_owned(),     serde_json::json!(a.noise_ratio));
                        obj.insert("anomaly_ratio".to_owned(),   serde_json::json!(a.anomaly_ratio));
                        obj.insert("seed".to_owned(),            serde_json::json!(a.seed));
                    }
                }
                o
            };

            // Retention sweeper stats — always present so operators
            // can tell at a glance whether retention is running or
            // turned off.  Values are atomic counters maintained by
            // `bdslib::retention::record_run` after every sweep.
            // Aggregate self-healing verdict.  Folds every registered
            // health source (background-task heartbeats, quarantined
            // shards, pool health, …) into one healthy/degraded/failed
            // verdict.  The full per-source breakdown lives at
            // `v2/health` — this is the headline for the status tile.
            let health_block = {
                let reg = bdslib::health::registry();
                let verdict = reg.verdict();
                let snap = reg.snapshot();
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let n_degraded = snap.iter()
                    .filter(|r| matches!(r.effective(now),
                        bdslib::health::HealthStatus::Degraded(_)))
                    .count();
                let n_failed = snap.iter()
                    .filter(|r| matches!(r.effective(now),
                        bdslib::health::HealthStatus::Failed(_)))
                    .count();
                serde_json::json!({
                    "status":     verdict.label(),
                    "reason":     verdict.reason(),
                    "n_sources":  snap.len(),
                    "n_degraded": n_degraded,
                    "n_failed":   n_failed,
                })
            };

            // DuckDB connection-pool health.  `checkout_timeouts` is
            // the lifetime count of `pool.get()` calls that hit the
            // 10 s ceiling — non-zero means a pool ran out of
            // connections under load (raise `pool_size`).
            let pool_block = serde_json::json!({
                "checkout_timeouts": bdslib::storageengine::pool_checkout_timeouts(),
            });

            // Self-healing — shard quarantine + rebuild activity.
            // `quarantined_now` is how many shards are currently
            // isolated awaiting repair; the lifetime counters show
            // total quarantine / heal / unhealable decisions.  All
            // healthy = every counter at 0 (or heals == quarantines).
            let self_healing_block = {
                let t = bdslib::shardhealth::tracker();
                let quarantined_now = bdslib::get_db()
                    .ok()
                    .and_then(|db| db.list_quarantined_shards().ok())
                    .map(|v| v.len())
                    .unwrap_or(0);
                serde_json::json!({
                    "quarantined_now":     quarantined_now,
                    "quarantines_total":   t.quarantines_total(),
                    "heals_total":         t.heals_total(),
                    "unhealable_total":    t.unhealable_total(),
                    "recreations_total":   t.recreations_total(),
                    "breaker_trips_total": t.breaker_trips_total(),
                })
            };

            // Ingest flusher liveness.  A dead flusher used to be
            // invisible until shutdown; these counters make it
            // observable.  `alive < configured` means the supervisor
            // is mid-respawn or a flusher is wedged; non-zero
            // `restarts_total` means a flusher panicked at least once.
            let ingest_flushers_block = {
                use std::sync::atomic::Ordering;
                let s = crate::server::add::stats();
                serde_json::json!({
                    "alive":           s.alive.load(Ordering::Relaxed),
                    "configured":      s.configured.load(Ordering::Relaxed),
                    "restarts_total":  s.restarts_total.load(Ordering::Relaxed),
                    "records_dropped": s.records_dropped.load(Ordering::Relaxed),
                })
            };

            // Rebalancer sweeper stats — present even when disabled
            // so operators can tell at a glance whether the task ran.
            // Atomic counters maintained by
            // `bdslib::rebalancer::record_run` after every tick.
            let rebalancer_block = {
                use std::sync::atomic::Ordering;
                let s = bdslib::rebalancer::stats();
                serde_json::json!({
                    "records_replicated_lifetime": s.records_replicated_lifetime.load(Ordering::Relaxed),
                    "records_replicated_last_run": s.records_replicated_last_run.load(Ordering::Relaxed),
                    "records_examined_lifetime":   s.records_examined_lifetime.load(Ordering::Relaxed),
                    "records_examined_last_run":   s.records_examined_last_run.load(Ordering::Relaxed),
                    "batches_examined_lifetime":   s.batches_examined_lifetime.load(Ordering::Relaxed),
                    "batches_examined_last_run":   s.batches_examined_last_run.load(Ordering::Relaxed),
                    "paused_for_lag_lifetime":     s.paused_for_lag_lifetime.load(Ordering::Relaxed),
                    "errors_lifetime":             s.errors_lifetime.load(Ordering::Relaxed),
                    "last_run_ts":                 s.last_run_ts.load(Ordering::Relaxed),
                    "last_run_ms":                 s.last_run_ms.load(Ordering::Relaxed),
                })
            };

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

            // Compact perf headline — just the series operators reach
            // for first when triaging.  Full snapshot lives at v2/perf.
            // Zeros when a series has never been touched.
            let perf_block = {
                let reg = bdslib::perf::registry();
                // Aggregate fanout/replicate.method.* into a single max p95
                // so the dashboard tile shows one number even with many
                // methods.  Per-method drill-down stays in v2/perf.
                let mut max_fanout_p95 = 0u64;
                let mut max_replicate_p95 = 0u64;
                for (name, s) in reg.snapshot_all() {
                    if name.starts_with("fanout.method.")    { max_fanout_p95    = max_fanout_p95.max(s.p95_us); }
                    if name.starts_with("replicate.method.") { max_replicate_p95 = max_replicate_p95.max(s.p95_us); }
                }
                let flush = reg.snapshot_one("ingest.flush");
                let lag   = reg.snapshot_one("ingest.lag");
                serde_json::json!({
                    "ingest_flush_p50_us":   flush.p50_us,
                    "ingest_flush_p95_us":   flush.p95_us,
                    "ingest_flush_p99_us":   flush.p99_us,
                    "ingest_flush_n_total":  flush.n_total,
                    "ingest_lag_p50_us":     lag.p50_us,
                    "ingest_lag_p95_us":     lag.p95_us,
                    "fanout_p95_us_max":     max_fanout_p95,
                    "replicate_p95_us_max":  max_replicate_p95,
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
                "rebalancer":         rebalancer_block,
                "ingest_flushers":    ingest_flushers_block,
                "pool":               pool_block,
                "self_healing":       self_healing_block,
                "health":             health_block,
                "dev_data":           dev_data_block,
                "perf":               perf_block,
            });

            log::debug!("v2/status: done");
            Ok::<serde_json::Value, jsonrpsee::types::ErrorObject>(value)
        })
        .unwrap();
}
