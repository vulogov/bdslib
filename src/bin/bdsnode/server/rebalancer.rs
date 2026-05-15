//! Background tokio task — data rebalancer for sharded telemetry.
//!
//! Mirrors the structure of [`server::retention`] (tick loop + oneshot
//! shutdown + `spawn_blocking` for DuckDB), with one difference: a
//! rebalance pass is *ephemeral*.  The sweep future is raced against
//! the shutdown signal, so a node going down mid-sweep drops the pass
//! immediately — cancelling any in-flight peer push — instead of
//! blocking shutdown until the sweep finishes.  The sweep is
//! idempotent (every tick re-scans the whole catalog and only pushes
//! under-replicated records), so unfinished work simply resumes on the
//! next start.
//!
//! Disabled-by-default (see `RebalancerConfig::disabled`).  Operators
//! opt in by setting `rebalancer.enabled = true` in `bds.hjson`.
//!
//! See `Documentation/REBALANCER.md` for the architectural overview
//! and operator guide.

use bdslib::cluster::replication::call_peer_v2_with_timeout;
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
    // Apply cache tuning to the process-wide have-cache.  Idempotent
    // — restarting the rebalancer with new cfg picks up cleanly.
    crate::server::rebalancer_state::configure(
        cfg.have_cache_capacity,
        std::time::Duration::from_secs(cfg.have_cache_ttl_secs),
    );
    log::info!(
        "[rebalancer] started — interval={:?} batch_size={} max_per_run={} \
         min_rf={:?} pause_lag={}ms pause_flush={}ms \
         have_cache={}/{}s slow_peer_skip_after={} push_concurrency={}",
        cfg.interval, cfg.batch_size, cfg.max_per_run,
        cfg.min_replication_factor,
        cfg.pause_if_ingest_lag_p95_ms,
        cfg.pause_if_ingest_flush_p95_ms,
        cfg.have_cache_capacity, cfg.have_cache_ttl_secs,
        cfg.slow_peer_skip_after, cfg.push_concurrency,
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
    // Health source — stale window 3× the sweep interval.
    bdslib::health::register(
        "rebalancer",
        (cfg.interval.as_secs().saturating_mul(3)).max(60),
    );
    loop {
        // Phase 1 — wait out the interval, but bail immediately if
        // shutdown arrives between ticks.
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                log::debug!("[rebalancer] shutdown signal received between ticks — stopping");
                break;
            }
            _ = tokio::time::sleep(cfg.interval) => {}
        }

        bdslib::health::heartbeat("rebalancer");

        // Phase 2 — run one sweep, raced against the shutdown signal.
        // A rebalance pass is *ephemeral*: if the node is going down
        // mid-sweep we drop the in-flight future right here, which
        // cancels any in-flight peer push instead of blocking shutdown
        // until the sweep (and its generous per-push timeout) finishes.
        // The sweep is idempotent, so whatever was left is simply
        // re-scanned on the next start.
        //
        // `supervise::tick` panic-isolates the sweep — a panic in
        // scan_pass / process_batch must not kill the rebalancer loop.
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                log::info!(
                    "[rebalancer] shutdown during sweep — aborted in-flight rebalance; \
                     unfinished shards will be re-scanned on next start"
                );
                break;
            }
            _ = crate::server::supervise::tick("rebalancer", run_one_tick(&cfg, &cluster)) => {}
        }
    }
}

/// One scan pass.  Logs the outcome via `record_run`.
async fn run_one_tick(cfg: &RebalancerConfig, cluster: &Arc<Cluster>) {
    let started = std::time::Instant::now();

    // Throttle: skip the tick when ingest is visibly backed up.
    // Reads p95 from the perf registry; falls back to "run anyway"
    // when fewer than 20 samples have accumulated (cold start).  Two
    // independent signals — either can pause:
    //   - ingest.lag p95   — queue wait time
    //   - ingest.flush p95 — actual write time
    // Flush matters even when lag is fine: it means DuckDB/Tantivy/
    // HNSW are saturated, and the rebalancer's writes go through the
    // same engines on the receiver — competing with live ingest is
    // the worst possible thing we can do in that state.
    let perf = bdslib::perf::registry();
    let pause_check = |series: &str, threshold_ms: u64| -> Option<u64> {
        if threshold_ms == 0 { return None; }
        let p95_us = perf.p95_us(series, 20)?;
        let p95_ms = p95_us / 1_000;
        if p95_ms > threshold_ms { Some(p95_ms) } else { None }
    };
    let pause_reason = pause_check("ingest.lag",   cfg.pause_if_ingest_lag_p95_ms)
        .map(|p95| ("ingest.lag",   p95, cfg.pause_if_ingest_lag_p95_ms))
        .or_else(|| pause_check("ingest.flush", cfg.pause_if_ingest_flush_p95_ms)
            .map(|p95| ("ingest.flush", p95, cfg.pause_if_ingest_flush_p95_ms)));
    if let Some((series, p95_ms, threshold_ms)) = pause_reason {
        log::info!(
            "[rebalancer] skipping tick — {series} p95 = {p95_ms}ms > {threshold_ms}ms"
        );
        let report = ScanReport {
            paused_for_lag: true,
            elapsed_ms: started.elapsed().as_millis() as u64,
            ..Default::default()
        };
        record_run(&report);
        return;
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

    // Per-sweep state — slow-peer skip list.  Reset between sweeps so
    // a peer that recovered isn't stuck on the skip list indefinitely.
    let mut sweep_state = crate::server::rebalancer_state::SweepState::new(
        cfg.slow_peer_skip_after,
    );

    // Drop expired entries from the process-wide have-cache once per
    // sweep — amortises the cost across all batches.
    crate::server::rebalancer_state::cache().evict_expired();

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
                cfg, cluster, &alive_peer_ids, self_id, min_rf,
                chunk, &mut sweep_state,
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

    if sweep_state.skipped_count() > 0 {
        log::info!(
            "[rebalancer] sweep finished — {} slow peer(s) skipped after \
             {} probe timeout(s); have_cache entries: {}",
            sweep_state.skipped_count(),
            cfg.slow_peer_skip_after,
            crate::server::rebalancer_state::cache().len(),
        );
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
/// 1. consult the cross-sweep have-cache for known `(peer, record)`
///    answers; only probe peers we don't yet have a fresh answer from.
/// 2. probe `v2/cluster.has_records` (write-class deadline) against
///    every non-slow peer; spawn fast-p95 peers first so their
///    responses arrive first and quorum-based early-cancel skips
///    waiting on stragglers.
/// 3. for each under-replicated record, fetch + push to picked peers
///    in bounded-concurrency parallel (`cfg.push_concurrency`).
///
/// Returns the number of successful per-record pushes.  Per-push
/// failures are logged at DEBUG (they retry next tick); peer-skip
/// transitions are logged once-per-peer at INFO via [`SweepState`].
async fn process_batch(
    cfg:            &RebalancerConfig,
    cluster:        &Arc<Cluster>,
    alive_peer_ids: &[Uuid],
    self_id:        Uuid,
    min_rf:         usize,
    chunk:          &[Uuid],
    sweep:          &mut crate::server::rebalancer_state::SweepState,
) -> anyhow::Result<u64> {
    if chunk.is_empty() {
        return Ok(0);
    }

    // Shared write-class deadline — same envelope used for replicate.
    // Default `peer_rpc_timeout=2s` × 5 → 10s, floored 15s.
    let write_timeout = std::time::Duration::from_secs(
        cluster.config.peer_rpc_timeout_secs.saturating_mul(5).max(15),
    );

    let cache = crate::server::rebalancer_state::cache();

    // Map: record_id → set of peer node_ids known to hold the record.
    // Seeded from the cache so subsequent probes only have to fill the
    // gaps.  `self_id` is implicit (the rebalancer always scans local).
    let mut have_map: std::collections::HashMap<Uuid, HashSet<Uuid>> =
        chunk.iter().map(|id| (*id, HashSet::new())).collect();

    // Resolve peer URLs + drop slow-skipped peers.  Slow peers contribute
    // nothing to `have_map` for this sweep — pick_peers_to_push will
    // therefore treat them as targets if they're alive_peer_ids members,
    // which is fine: a genuine replicate to a recovered peer will succeed
    // and re-warm the cache.
    let peers_full: Vec<(Uuid, String)> = {
        let pt = cluster.peers.read();
        pt.alive().into_iter()
            .filter(|p| alive_peer_ids.contains(&p.node_id) && !sweep.is_slow(p.node_id))
            .map(|p| (p.node_id, p.url))
            .collect()
    };

    // ── 1. cache consultation: build per-peer "unknown" probe lists ───
    //
    // For each peer, partition `chunk` into "already known from cache"
    // (seeded into have_map for positives, dropped silently for known-
    // absent) and "unknown" (the only IDs we actually probe).  A peer
    // whose unknown set is empty doesn't get probed at all this batch.
    let perf = bdslib::perf::registry();
    let mut per_peer_probes: Vec<(Uuid, String, Vec<String>, u64)> = Vec::with_capacity(peers_full.len());
    for (peer_id, url) in &peers_full {
        let mut unknown_strs: Vec<String> = Vec::with_capacity(chunk.len());
        for &id in chunk {
            match cache.get(*peer_id, id) {
                Some(true)  => { have_map.get_mut(&id).map(|s| s.insert(*peer_id)); }
                Some(false) => { /* known-absent — don't include in unknown */ }
                None        => unknown_strs.push(id.to_string()),
            }
        }
        if unknown_strs.is_empty() {
            // Whole batch served from cache for this peer — no probe.
            continue;
        }
        // Per-peer p95 for fast-first spawn ordering.  Missing series
        // (peer never probed) sorts to 0 = scheduled first.
        let p95 = perf.p95_us(&format!("fanout.peer.{peer_id}"), 20).unwrap_or(0);
        per_peer_probes.push((*peer_id, url.clone(), unknown_strs, p95));
    }
    // Fast peers first — JoinSet still surfaces responses in
    // arrival order, but spawning fast peers first reduces the chance
    // we end up waiting on a slow peer when quorum is reachable from
    // the fast ones alone.
    per_peer_probes.sort_by_key(|(_, _, _, p95)| *p95);

    // ── 2. probe with quorum-based early exit ────────────────────────
    //
    // Targets per record can only DECREASE as we probe more peers; once
    // `pick_peers_to_push` returns empty for every chunk record, more
    // probes are wasted work.  Abort the JoinSet at that point.
    let mut probe_set: tokio::task::JoinSet<(
        Uuid,             // peer_id
        Vec<String>,      // probed IDs (so we can update the cache below)
        Result<JsonValue, String>,
    )> = tokio::task::JoinSet::new();
    for (peer_id, url, ids, _p95) in per_peer_probes {
        let cluster = cluster.clone();
        let to      = write_timeout;
        let params  = json!({ "ids": ids.clone() });
        probe_set.spawn(async move {
            let started = std::time::Instant::now();
            let res = call_peer_v2_with_timeout(
                &cluster, &url, "v2/cluster.has_records", &params, to,
            ).await;
            let elapsed_us = started.elapsed().as_micros() as u64;
            bdslib::perf::record_us("fanout.method.v2/cluster.has_records", elapsed_us);
            bdslib::perf::record_us(&format!("fanout.peer.{peer_id}"),       elapsed_us);
            (peer_id, ids, res.map_err(|e| e.to_string()))
        });
    }

    while let Some(jr) = probe_set.join_next().await {
        let (peer_id, probed_ids, res) = match jr {
            Ok(t)  => t,
            Err(e) => {
                log::warn!("[rebalancer] has_records probe task panicked: {e:?}");
                continue;
            }
        };
        let v = match res {
            Ok(v)  => v,
            Err(_) => {
                if sweep.note_timeout(peer_id) {
                    log::info!(
                        "[rebalancer] peer {peer_id} skipped for rest of sweep \
                         after {} probe timeout(s)",
                        cfg.slow_peer_skip_after,
                    );
                }
                continue;
            }
        };
        // Parse present IDs into a set so we can compute the absent
        // complement for cache-update purposes in O(probed × 1).
        let present_set: HashSet<Uuid> = v.get("present")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter()
                .filter_map(|s| s.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                .collect())
            .unwrap_or_default();
        for sid in &probed_ids {
            let Ok(uuid) = Uuid::parse_str(sid) else { continue };
            let has = present_set.contains(&uuid);
            cache.insert(peer_id, uuid, has);
            if has {
                if let Some(set) = have_map.get_mut(&uuid) {
                    set.insert(peer_id);
                }
            }
        }
        // Early-cancel: are all records at min_rf or higher?  `self_id`
        // is always a holder (we just read this record locally), hence
        // the `+ 1`.
        let all_at_quorum = chunk.iter().all(|id| {
            have_map.get(id).map(|s| s.len() + 1).unwrap_or(1) >= min_rf
        });
        if all_at_quorum {
            probe_set.abort_all();
            break;
        }
    }

    // ── 3. push under-replicated records to picked peers ─────────────
    //
    // Build a flat push plan first so we can dispatch with bounded
    // concurrency.  Local fetches stay serial (DuckDB pool friendly);
    // the network pushes run via a Semaphore-bounded JoinSet.
    //
    // `pick_peers_to_push` is deterministic — sorting `alive_peer_ids`
    // up-front keeps the per-record target set stable across batches
    // so re-probes hit the cache.
    use tokio::sync::Semaphore;

    struct Push {
        record_id: Uuid,
        target_id: Uuid,
        url:       String,
        doc:       JsonValue,
    }

    let mut plan: Vec<Push> = Vec::new();
    for &record_id in chunk {
        let have = have_map.get(&record_id).cloned().unwrap_or_default();
        let targets = pick_peers_to_push(&have, alive_peer_ids, self_id, min_rf);
        if targets.is_empty() {
            continue;
        }
        let doc = match fetch_record_local(record_id).await {
            Ok(Some(d)) => d,
            Ok(None) => {
                // Race: the catalog said the shard had this ID but the
                // observability layer doesn't.  Concurrent delete; skip.
                log::debug!("[rebalancer] {record_id} no longer present locally; skipping");
                continue;
            }
            Err(e) => {
                log::warn!("[rebalancer] fetch_record_local({record_id}): {e}");
                continue;
            }
        };
        let alive_now = cluster.peers.read().alive();
        for target_id in targets {
            // Resolve the URL — peer may have gone Dead between the
            // snapshot and now, OR slipped onto the slow-skip list
            // during this sweep.  Both cases are clean skips: AE
            // will reconcile what we missed.
            if sweep.is_slow(target_id) { continue; }
            let url = match alive_now.iter().find(|p| p.node_id == target_id).map(|p| p.url.clone()) {
                Some(u) => u,
                None    => continue,
            };
            plan.push(Push { record_id, target_id, url, doc: doc.clone() });
        }
    }

    if plan.is_empty() {
        return Ok(0);
    }

    // Tier-3 (#8) — pre-warm hint.  Group the plan by (target_id) and
    // for each peer send ONE `v2/cluster.shard_warm` with the unique
    // record timestamps the push phase is about to hit.  The receiver
    // opens those shards (cold-shard cost: DuckDB pool checkout +
    // Tantivy/HNSW init) once, in parallel with our push-permit
    // acquisition — so by the time the first push lands the target
    // shards are typically already warm.  Fire-and-forget: a failed
    // warm never blocks the eventual replicate_record (the push
    // still works, it just pays the cold-open cost the old way).
    {
        use std::collections::BTreeMap;
        let mut by_target: BTreeMap<Uuid, (String, std::collections::BTreeSet<i64>)> =
            BTreeMap::new();
        for p in &plan {
            let ts = p.doc.get("timestamp")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let entry = by_target
                .entry(p.target_id)
                .or_insert_with(|| (p.url.clone(), std::collections::BTreeSet::new()));
            entry.1.insert(ts);
        }
        // Warm RPC gets the gossip-ping budget — it's a cheap "open the
        // shard" call; a slow one means the shard would have been slow
        // for the push anyway and we shouldn't double-pay the wait.
        let warm_timeout = std::time::Duration::from_secs(
            cluster.config.peer_rpc_timeout_secs.max(2),
        );
        for (target_id, (url, ts_set)) in by_target {
            let timestamps: Vec<i64> = ts_set.into_iter().collect();
            let params = json!({ "timestamps": timestamps });
            let cluster = cluster.clone();
            let url = url.clone();
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let res = call_peer_v2_with_timeout(
                    &cluster, &url, "v2/cluster.shard_warm", &params, warm_timeout,
                ).await;
                let elapsed_us = started.elapsed().as_micros() as u64;
                bdslib::perf::record_us("rebalancer.shard_warm", elapsed_us);
                bdslib::perf::record_us(
                    &format!("rebalancer.shard_warm.peer.{target_id}"), elapsed_us);
                if let Err(e) = res {
                    log::debug!(
                        "[rebalancer] shard_warm → {url}: {e} (push will pay cold-open)"
                    );
                }
            });
        }
    }

    // Bounded-concurrency push.  `push_concurrency=1` reproduces the
    // pre-Tier-2 serial behaviour; default 4 overlaps cold-shard-open
    // cost across receivers.
    let semaphore = Arc::new(Semaphore::new(cfg.push_concurrency.max(1)));
    let cluster_arc = cluster.clone();
    let cache_ref = cache; // 'static reference, cheap to capture
    let mut push_set: tokio::task::JoinSet<bool> = tokio::task::JoinSet::new();
    for push in plan {
        let sem = semaphore.clone();
        let cl  = cluster_arc.clone();
        push_set.spawn(async move {
            let _permit = match sem.acquire_owned().await {
                Ok(p)  => p,
                Err(_) => return false, // semaphore closed (shouldn't happen)
            };
            let started = std::time::Instant::now();
            let params  = json!({ "record": push.doc });
            let res = call_peer_v2_with_timeout(
                &cl, &push.url, "v2/cluster.replicate_record", &params, write_timeout,
            ).await;
            let elapsed_us = started.elapsed().as_micros() as u64;
            bdslib::perf::record_us("rebalancer.replicate_one", elapsed_us);
            match res {
                Ok(resp) => {
                    // We just confirmed (or pushed) this record to the target.
                    cache_ref.insert(push.target_id, push.record_id, true);
                    resp.get("was_new").and_then(|x| x.as_bool()).unwrap_or(false)
                }
                Err(e) => {
                    log::debug!(
                        "[rebalancer] replicate {} → {}: {e} (will retry next tick)",
                        push.record_id, push.url,
                    );
                    false
                }
            }
        });
    }

    let mut n_replicated: u64 = 0;
    while let Some(jr) = push_set.join_next().await {
        match jr {
            Ok(true)  => n_replicated += 1,
            Ok(false) => {}
            Err(e)    => log::warn!("[rebalancer] push task panicked: {e:?}"),
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
