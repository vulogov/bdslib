//! Background tokio task — data rebalancer for sharded telemetry.
//!
//! Mirrors the structure of [`server::retention`] (tick loop + oneshot
//! shutdown + `spawn_blocking` for DuckDB).  Each tick performs at
//! most one scan pass; cancellation is checked after every atomic
//! write to a peer so a long sweep can be interrupted cleanly.
//!
//! Disabled-by-default (see `RebalancerConfig::disabled`).  Operators
//! opt in by setting `rebalancer.enabled = true` in `bds.hjson`.
//!
//! See `Documentation/REBALANCER.md` for the architectural overview
//! and operator guide.

use bdslib::cluster::fanout;
use bdslib::cluster::replication::call_peer_v2;
use bdslib::cluster::Cluster;
use bdslib::rebalancer::{
    pick_peers_to_push, record_run, RebalancerConfig, ScanReport,
};
use serde_json::{json, Value as JsonValue};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Handle returned by [`start`].  Drop or call [`Handle::stop`] to end
/// the task; [`stop`] also awaits the join so callers can sequence
/// shutdown deterministically.
pub struct Handle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    task:        Option<tokio::task::JoinHandle<()>>,
}

impl Handle {
    fn disabled() -> Self { Self { shutdown_tx: None, task: None } }

    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.task.take() {
            if let Err(e) = t.await {
                log::error!("[rebalancer] task panicked on shutdown: {e:?}");
            }
        }
    }
}

/// Spawn the rebalancer task.  Returns a disabled-noop [`Handle`]
/// when `cfg.enabled = false` or when cluster mode is off (a
/// standalone node has nothing to rebalance).
pub fn start(cfg: RebalancerConfig) -> Handle {
    if !cfg.enabled {
        log::info!("[rebalancer] disabled (rebalancer.enabled=false)");
        return Handle::disabled();
    }
    // Resolve the cluster handle once at startup.  When standalone,
    // there's no peer set to balance against.
    let cluster = match bdslib::get_db().ok().and_then(|db| db.cluster().cloned()) {
        Some(c) => c,
        None    => {
            log::info!("[rebalancer] standalone node — nothing to rebalance");
            return Handle::disabled();
        }
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(run(cfg.clone(), cluster, shutdown_rx));
    log::info!(
        "[rebalancer] started — interval={:?} batch_size={} max_per_run={} \
         min_rf={:?} pause_lag={}ms",
        cfg.interval, cfg.batch_size, cfg.max_per_run,
        cfg.min_replication_factor, cfg.pause_if_ingest_lag_p95_ms,
    );
    Handle {
        shutdown_tx: Some(shutdown_tx),
        task:        Some(task),
    }
}

async fn run(
    cfg:             RebalancerConfig,
    cluster:         Arc<Cluster>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                log::debug!("[rebalancer] shutdown signal received — stopping");
                break;
            }
            _ = tokio::time::sleep(cfg.interval) => {
                run_one_tick(&cfg, &cluster).await;
            }
        }
    }
}

/// One scan pass.  Logs the outcome via `record_run`.
async fn run_one_tick(cfg: &RebalancerConfig, cluster: &Arc<Cluster>) {
    let started = std::time::Instant::now();

    // Throttle: skip the tick when ingest is visibly backed up.
    // Reads p95 from the perf registry; falls back to "run anyway"
    // when fewer than 20 samples have accumulated (cold start).
    if cfg.pause_if_ingest_lag_p95_ms > 0 {
        if let Some(p95_us) = bdslib::perf::registry().p95_us("ingest.lag", 20) {
            let p95_ms = p95_us / 1_000;
            if p95_ms > cfg.pause_if_ingest_lag_p95_ms {
                log::info!(
                    "[rebalancer] skipping tick — ingest.lag p95 = {p95_ms}ms \
                     > pause_if_ingest_lag_p95_ms = {}ms",
                    cfg.pause_if_ingest_lag_p95_ms
                );
                let report = ScanReport {
                    paused_for_lag: true,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    ..Default::default()
                };
                record_run(&report);
                return;
            }
        }
    }

    match scan_pass(cfg, cluster).await {
        Ok(mut report) => {
            report.elapsed_ms = started.elapsed().as_millis() as u64;
            log::debug!(
                "[rebalancer] tick done — examined={} replicated={} batches={} errors={} ms={}",
                report.records_examined, report.records_replicated,
                report.batches_examined, report.errors, report.elapsed_ms,
            );
            record_run(&report);
        }
        Err(e) => {
            log::warn!("[rebalancer] scan_pass failed: {e}");
            let report = ScanReport {
                errors: 1,
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..Default::default()
            };
            record_run(&report);
        }
    }
}

/// One sweep across all shards.  Returns a partial report on
/// per-batch failures (we count and continue) and a hard error only
/// when something fundamental fails (cluster missing, DB missing).
async fn scan_pass(
    cfg:     &RebalancerConfig,
    cluster: &Arc<Cluster>,
) -> anyhow::Result<ScanReport> {
    // Resolve the min_rf: explicit override or inherit cluster.replication_factor.
    let min_rf = cfg.min_replication_factor
        .unwrap_or(cluster.config.replication_factor)
        .max(1);

    // Get an up-front snapshot of Alive peer IDs.  Peer state may
    // change mid-scan; we don't refresh because the alternative
    // (re-reading every iteration) would create a measurement-vs-
    // action race where we observe a peer Alive on the probe and
    // Dead on the write.  Snapshot once is the right tradeoff.
    let alive_peer_ids: Vec<Uuid> = cluster.peers.read().alive()
        .iter().map(|p| p.node_id).collect();
    if alive_peer_ids.is_empty() {
        log::debug!("[rebalancer] no Alive peers; nothing to do");
        return Ok(ScanReport::default());
    }

    let self_id = cluster.node_id;

    // Snapshot the shard catalog.  Per-shard reads happen one at a
    // time on spawn_blocking so we never hold a DB handle across an
    // await point.
    let shard_infos = {
        let db = bdslib::get_db().map_err(|e| anyhow::anyhow!("{e}"))?;
        let cache_ref = db.cache().clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            cache_ref.info().list_all().map_err(|e| anyhow::anyhow!("{e}"))
        }).await.map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))??
    };

    let mut report = ScanReport::default();
    let mut budget_remaining = cfg.max_per_run;

    'shards: for info in shard_infos {
        if budget_remaining == 0 { break 'shards; }

        // ── enumerate IDs in this shard ───────────────────────────────
        let start_time = info.start_time;
        let end_time   = info.end_time;
        let ids = match list_ids_blocking(start_time, end_time).await {
            Ok(v)  => v,
            Err(e) => {
                log::warn!("[rebalancer] list_ids in shard {start_time:?}..{end_time:?}: {e}");
                report.errors += 1;
                continue;
            }
        };

        // Iterate in batches of `batch_size`, capped by remaining budget.
        for chunk in ids.chunks(cfg.batch_size) {
            if budget_remaining == 0 { break 'shards; }
            let take = chunk.len().min(budget_remaining);
            let chunk = &chunk[..take];
            budget_remaining -= take;
            report.batches_examined += 1;
            report.records_examined += take as u64;

            // ── one scan batch ────────────────────────────────────────
            let started = std::time::Instant::now();
            let batch_outcome = process_batch(
                cluster, &alive_peer_ids, self_id, min_rf, chunk
            ).await;
            bdslib::perf::record_us("rebalancer.scan_batch",
                started.elapsed().as_micros() as u64);

            match batch_outcome {
                Ok(n_replicated) => {
                    report.records_replicated += n_replicated;
                }
                Err(e) => {
                    log::warn!("[rebalancer] batch failed: {e}");
                    report.errors += 1;
                }
            }
        }
    }

    Ok(report)
}

/// Spawn-blocking wrapper over `list_ids_by_time_range`.
async fn list_ids_blocking(
    start: std::time::SystemTime,
    end:   std::time::SystemTime,
) -> anyhow::Result<Vec<Uuid>> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Uuid>> {
        let db = bdslib::get_db().map_err(|e| anyhow::anyhow!("{e}"))?;
        let cache = db.cache();
        let shard = cache.shard(start).map_err(|e| anyhow::anyhow!("{e}"))?;
        let obs = shard.observability();
        obs.list_ids_by_time_range(start, end).map_err(|e| anyhow::anyhow!("{e}"))
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))?
}

/// One batch's work:
/// 1. fan_out_v2 `v2/cluster.has_records` to every Alive peer
/// 2. union the returned ID sets per record
/// 3. for each under-replicated record, push to picked peers
///
/// Returns the number of successful per-record pushes.  Per-push
/// failures are logged at WARN and counted as errors at the caller
/// level (caller increments `report.errors` per failed batch).
async fn process_batch(
    cluster:        &Arc<Cluster>,
    alive_peer_ids: &[Uuid],
    self_id:        Uuid,
    min_rf:         usize,
    chunk:          &[Uuid],
) -> anyhow::Result<u64> {
    if chunk.is_empty() {
        return Ok(0);
    }

    // ── 1. probe peers ────────────────────────────────────────────────
    let id_strings: Vec<String> = chunk.iter().map(|u| u.to_string()).collect();
    let fanout_res = fanout::fan_out_v2(
        cluster, "v2/cluster.has_records", json!({ "ids": id_strings })
    ).await;

    // Build per-record "who has it" set from successful peer responses.
    // Map: record_id → set of peer node_ids that reported present.
    let mut have_map: std::collections::HashMap<Uuid, HashSet<Uuid>> =
        chunk.iter().map(|id| (*id, HashSet::new())).collect();
    for peer_res in &fanout_res.responses {
        let Ok(v) = &peer_res.result else { continue };
        let Some(arr) = v.get("present").and_then(|x| x.as_array()) else { continue };
        let peer_id = peer_res.peer.node_id;
        for s in arr {
            if let Some(s) = s.as_str() {
                if let Ok(uuid) = Uuid::parse_str(s) {
                    if let Some(set) = have_map.get_mut(&uuid) {
                        set.insert(peer_id);
                    }
                }
            }
        }
    }

    // ── 2. for each record, decide who to push to ─────────────────────
    let mut n_replicated: u64 = 0;
    for &record_id in chunk {
        let have = have_map.get(&record_id).cloned().unwrap_or_default();
        let targets = pick_peers_to_push(&have, alive_peer_ids, self_id, min_rf);
        if targets.is_empty() {
            continue;
        }

        // Fetch the record locally — we need the full body to push.
        let doc = match fetch_record_local(record_id).await {
            Ok(Some(d)) => d,
            Ok(None) => {
                // Race: the catalog said the shard had this ID but the
                // observability layer doesn't.  Could be a concurrent
                // delete.  Skip and move on.
                log::debug!("[rebalancer] {record_id} no longer present locally; skipping");
                continue;
            }
            Err(e) => {
                log::warn!("[rebalancer] fetch_record_local({record_id}): {e}");
                continue;
            }
        };

        // ── 3. push to each target peer ─────────────────────────────
        for target_id in targets {
            // Resolve the URL — peer may have transitioned to Dead
            // between the alive snapshot and now; skip cleanly.
            let url = match cluster.peers.read().alive()
                .iter().find(|p| p.node_id == target_id)
                .map(|p| p.url.clone())
            {
                Some(u) => u,
                None    => continue,
            };

            let started = std::time::Instant::now();
            let params = json!({ "record": doc.clone() });
            match call_peer_v2(cluster, &url, "v2/cluster.replicate_record", &params).await {
                Ok(resp) => {
                    let was_new = resp.get("was_new").and_then(|x| x.as_bool()).unwrap_or(false);
                    if was_new {
                        n_replicated += 1;
                    }
                    bdslib::perf::record_us("rebalancer.replicate_one",
                        started.elapsed().as_micros() as u64);
                }
                Err(e) => {
                    log::warn!(
                        "[rebalancer] replicate {record_id} → {url}: {e} (will retry next tick)"
                    );
                }
            }
        }
    }

    Ok(n_replicated)
}

/// Spawn-blocking fetch of one record's full body.
async fn fetch_record_local(id: Uuid) -> anyhow::Result<Option<JsonValue>> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<Option<JsonValue>> {
        let db = bdslib::get_db().map_err(|e| anyhow::anyhow!("{e}"))?;
        // Probe every catalog-registered shard.  Same naive strategy
        // has_records_present uses — fine at small batch sizes.
        let cache = db.cache();
        for info in cache.info().list_all().map_err(|e| anyhow::anyhow!("{e}"))? {
            let shard = cache.shard(info.start_time).map_err(|e| anyhow::anyhow!("{e}"))?;
            let obs   = shard.observability();
            let docs  = obs.get_by_ids(&[id]).map_err(|e| anyhow::anyhow!("{e}"))?;
            if let Some(d) = docs.into_iter().next() {
                return Ok(Some(d));
            }
        }
        Ok(None)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))?
}
