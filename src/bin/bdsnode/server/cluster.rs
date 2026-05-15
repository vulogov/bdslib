//! Background tokio task that drives the cluster gossip loop and the periodic
//! liveness sweep.  Disabled when `cluster.enabled = false` (or absent).
//!
//! Bootstrap pass runs once at startup against `cluster.bootstrap` plus every
//! peer in the persisted `peers.json`, then the tick loop runs until shutdown.

use bdslib::cluster::{gossip, replication};
use bdslib::cluster::peer_table::PeerState;
use crate::server::supervise;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;
// http client lives on Cluster (shared with the fanout helper); we no longer
// build one per task here.

/// Process at most this many hints per peer per replay tick.  Bounds the
/// time we spend on a single peer's backlog so other peers still get a turn.
const HINT_BATCH_PER_PEER: usize = 100;

/// Handle returned by [`start`].  Drop or call [`Handle::stop`] to terminate.
pub struct Handle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    task:        Option<tokio::task::JoinHandle<()>>,
}

impl Handle {
    fn disabled() -> Self {
        Self { shutdown_tx: None, task: None }
    }

    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            if let Err(e) = task.await {
                log::error!("[cluster] gossip task panicked on shutdown: {e:?}");
            }
        }
    }
}

/// Spawn the cluster background task.  Returns a no-op handle when the
/// global database has no cluster layer (i.e. `cluster.enabled = false`).
pub fn start() -> Handle {
    let db = match bdslib::get_db() {
        Ok(db) => db,
        Err(e) => {
            log::warn!("[cluster] start: get_db failed: {e}");
            return Handle::disabled();
        }
    };
    let cluster = match db.cluster() {
        Some(c) => c.clone(),
        None    => {
            log::info!("[cluster] disabled (cluster.enabled=false)");
            return Handle::disabled();
        }
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(run(cluster, shutdown_rx));
    log::info!("[cluster] gossip task started");
    Handle { shutdown_tx: Some(shutdown_tx), task: Some(task) }
}

async fn run(
    cluster: Arc<bdslib::Cluster>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let cfg  = cluster.config.clone();
    let http = cluster.http.clone();

    // Startup bootstrap with bounded retry.
    //
    // The whole cluster commonly boots at once — peer URLs may not be
    // listening for a few seconds after this node starts.  Without
    // retry, the very first bootstrap pass sees "all targets refused
    // connection" and the node enters standalone mode for up to
    // `bootstrap_retry_interval_secs` (default 60 s) before the
    // steady-state retry loop fires.
    //
    // Loop here:
    //   - first pass always runs (preserves "standalone — no targets"
    //     behaviour and the existing first-pass log line);
    //   - if it joined ≥1 peer OR there were no candidates OR retry is
    //     disabled (`startup_bootstrap_max_wait_secs == 0`), we're done;
    //   - otherwise retry every `startup_bootstrap_retry_interval_secs`
    //     until either a pass joins a peer or the total wall-clock
    //     elapsed exceeds `startup_bootstrap_max_wait_secs`.
    //   - shutdown signal aborts the retry cleanly.
    //
    // The steady-state floating re-bootstrap loop further down still
    // runs as a safety net; this window just shortens the time-to-join
    // on coordinated cluster startup from O(bootstrap_retry_interval)
    // down to O(startup_bootstrap_retry_interval).
    {
        let max_wait = Duration::from_secs(cfg.startup_bootstrap_max_wait_secs);
        let retry_interval = Duration::from_secs(
            cfg.startup_bootstrap_retry_interval_secs.max(1),
        );
        let outcome = gossip::bootstrap(&cluster, &http).await;
        let attempted_first = outcome.attempted;
        let joined_first    = outcome.joined;
        record_bootstrap(&cluster, outcome);

        // Skip retry when: operator disabled it, there's nothing to
        // bootstrap against (standalone), or we already succeeded.
        if max_wait.is_zero() || attempted_first == 0 || joined_first > 0 {
            // nothing to do — first pass was authoritative
        } else {
            log::info!(
                "[cluster] startup bootstrap: 0/{attempted_first} target(s) reachable; \
                 retrying every {retry_interval:?} for up to {max_wait:?} \
                 (cluster may be booting)"
            );
            let started_at = Instant::now();
            let mut attempt: u32 = 1;
            'startup: loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        log::debug!("[cluster] shutdown during startup bootstrap retry");
                        return;
                    }
                    _ = tokio::time::sleep(retry_interval) => {}
                }
                attempt += 1;
                let elapsed = started_at.elapsed();
                let outcome = gossip::bootstrap(&cluster, &http).await;
                let joined = outcome.joined;
                record_bootstrap(&cluster, outcome);
                if joined > 0 {
                    log::info!(
                        "[cluster] startup bootstrap joined {joined} peer(s) \
                         on attempt #{attempt} after {elapsed:?}"
                    );
                    break 'startup;
                }
                if elapsed >= max_wait {
                    log::warn!(
                        "[cluster] startup bootstrap: no peer joined within {max_wait:?} \
                         ({attempt} attempts) — entering standalone; gossip will keep \
                         retrying every {}s",
                        cfg.bootstrap_retry_interval_secs,
                    );
                    break 'startup;
                }
            }
        }
    }

    let mut tick_no: u64 = 0;
    let interval = Duration::from_secs(cfg.gossip_interval_secs.max(1));
    let suspect  = Duration::from_secs(cfg.suspect_timeout_secs);
    let dead     = Duration::from_secs(cfg.dead_timeout_secs);

    // Hint replay runs on its own cadence, independent of gossip.
    let hint_interval = Duration::from_secs(cfg.hint_replay_interval_secs.max(1));
    let mut last_hint_tick = Instant::now();

    // Anti-entropy on a (typically much) longer cadence.
    let ae_interval = Duration::from_secs(cfg.antientropy_interval_secs.max(60));
    let mut last_ae_tick = Instant::now();

    // Floating re-bootstrap retry: when alive_count drops to 0 (we got
    // boxed out of the cluster), re-run the bootstrap pass.  In strict
    // mode the candidate set is just `cluster.bootstrap`; in floating
    // mode it's that URL plus every persisted peer.  Either way the
    // recovery probe alone can't help when the table itself is empty.
    let bootstrap_interval = Duration::from_secs(cfg.bootstrap_retry_interval_secs.max(10));
    let mut last_bootstrap_attempt = Instant::now();

    // Health source — a hung gossip loop means the node has gone
    // blind to peer state.  Stale window is 6× the gossip interval
    // (floored at 30s) so a couple of slow ticks don't false-positive.
    let gossip_interval_secs = cfg.gossip_interval_secs.max(1);
    bdslib::health::register(
        "cluster.gossip",
        (gossip_interval_secs.saturating_mul(6)).max(30),
    );

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                log::debug!("[cluster] shutdown signal — stopping gossip");
                break;
            }
            _ = tokio::time::sleep(interval) => {
                // Heartbeat BEFORE the tick body — proves the loop is
                // alive even if `supervise::tick` catches a panic in
                // this tick's work.
                bdslib::health::heartbeat("cluster.gossip");
                tick_no = tick_no.wrapping_add(1);

                // Panic-isolate the whole tick body (reliability #3):
                // a panic in gossip / recovery / hint-replay /
                // anti-entropy must not kill the gossip loop — that
                // would leave the node blind to peer state until
                // restart.  `supervise::tick` catches, logs, and
                // swallows; the timers below are loop-local so the
                // next tick simply carries on.
                supervise::tick("cluster", async {
                // Liveness sweep first so a peer that just transitioned to
                // Dead is excluded from the random-pick this tick.
                let changed = gossip::sweep(&cluster.peers, suspect, dead);
                if changed > 0 {
                    cluster.persist_peers_best_effort();
                    log::debug!("[cluster] sweep moved {changed} peer(s) between states");
                }

                match gossip::tick(&cluster, &http, tick_no).await {
                    gossip::GossipTickResult::NoAlivePeer       => {}  // standalone — quiet log
                    gossip::GossipTickResult::Pinged { .. }     => {}
                    gossip::GossipTickResult::Merged { peer, new_peers } => {
                        if new_peers > 0 {
                            log::info!("[cluster] merged {new_peers} new peer(s) from {peer}");
                        }
                    }
                    gossip::GossipTickResult::PingFailed  { peer, reason } => {
                        log::debug!("[cluster] ping failed: {peer}: {reason}");
                    }
                    gossip::GossipTickResult::PeersFailed { peer, reason } => {
                        log::debug!("[cluster] peers failed: {peer}: {reason}");
                    }
                }

                // Recovery probe — try to wake one Suspect/Dead peer.  Without
                // this, peers that were Dead at startup (or stuck Dead through
                // a previous outage) never get re-checked: pick_random_alive
                // ignores them and pick_random_non_alive isn't invoked anywhere
                // else.  No-op when every peer is already Alive.
                let _ = gossip::probe_recovery(&cluster, &http).await;

                // Floating re-bootstrap — only when we have zero Alive
                // peers (otherwise gossip handles things).  Fires at most
                // once per `bootstrap_retry_interval`.
                if cluster.peers.read().alive_count() == 0
                    && last_bootstrap_attempt.elapsed() >= bootstrap_interval
                {
                    last_bootstrap_attempt = Instant::now();
                    record_bootstrap(&cluster, gossip::bootstrap(&cluster, &http).await);
                }

                // Hint replay tick — runs independently of gossip cadence
                // so a slow gossip interval doesn't starve replication.
                if last_hint_tick.elapsed() >= hint_interval {
                    last_hint_tick = Instant::now();
                    let replayed = replay_hints(&cluster).await;
                    let mut s = cluster.stats.write();
                    s.last_hint_tick           = now_secs();
                    s.last_hint_tick_replayed  = replayed;
                }

                // Anti-entropy tick — pull-sync with one random Alive peer
                // for each fully-replicated store.  Catches up nodes that
                // were missing live entries (or missing tombstones for
                // entries the rest of the cluster has deleted).
                if last_ae_tick.elapsed() >= ae_interval {
                    last_ae_tick = Instant::now();
                    let outcome = antientropy_tick(&cluster).await;
                    let mut s = cluster.stats.write();
                    s.last_ae_tick             = now_secs();
                    s.last_ae_tick_pulled      = outcome.pulled;
                    s.last_ae_tick_tombstones  = outcome.tombstones;
                    s.last_ae_tick_pruned      = outcome.pruned;
                }
                }).await;  // end supervise::tick
            }
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Write the bootstrap outcome into `cluster.stats`.  The success
/// timestamp is left untouched when `joined == 0`, so operators can
/// still see when the last *successful* bootstrap was even after a run
/// of failed retries.
fn record_bootstrap(cluster: &Arc<bdslib::Cluster>, outcome: bdslib::cluster::gossip::BootstrapOutcome) {
    let mut s = cluster.stats.write();
    s.last_bootstrap_attempt   = now_secs();
    s.last_bootstrap_attempted = outcome.attempted as u64;
    s.last_bootstrap_joined    = outcome.joined as u64;
    if outcome.joined > 0 {
        s.last_bootstrap_success = now_secs();
    }
}

/// Drain hints for every peer that's currently Alive, retry them, and
/// delete the ones that succeed.  Hints older than `cluster.hint_max_age`
/// are pruned regardless of state.  Returns the total number of hints
/// successfully replayed in this tick (for the cluster.stats telemetry).
pub async fn replay_hints(cluster: &Arc<bdslib::Cluster>) -> u64 {
    // Always prune expired first so a flood of dead-peer hints can't
    // monopolise the backlog forever.
    if let Ok(dropped) = cluster.hints.prune_expired(cluster.config.hint_max_age_secs) {
        if dropped > 0 {
            log::info!("[cluster] dropped {dropped} expired hint(s)");
        }
    }

    let peers_with_hints = match cluster.hints.peers_with_hints() {
        Ok(v) => v,
        Err(e) => { log::warn!("[cluster] peers_with_hints: {e}"); return 0; }
    };
    if peers_with_hints.is_empty() { return 0; }
    let mut total_replayed = 0u64;

    // Build a quick {node_id -> url, state} lookup so we can skip
    // non-Alive peers in O(n) rather than re-locking per peer.
    let snapshot: std::collections::HashMap<uuid::Uuid, (String, PeerState)> = cluster
        .peers.read().snapshot().into_iter()
        .map(|p| (p.node_id, (p.url, p.state)))
        .collect();

    for peer_id in peers_with_hints {
        let (url, state) = match snapshot.get(&peer_id) {
            Some(p) => (p.0.clone(), p.1),
            None    => continue,  // peer dropped from table; hints will age out
        };
        if state != PeerState::Alive { continue; }

        let hints = match cluster.hints.drain_for_peer(peer_id, HINT_BATCH_PER_PEER) {
            Ok(v) => v,
            Err(e) => { log::warn!("[cluster] drain_for_peer({peer_id}): {e}"); continue; }
        };
        if hints.is_empty() { continue; }

        let mut succeeded: Vec<i64> = Vec::with_capacity(hints.len());
        for h in &hints {
            let params: serde_json::Value = match serde_json::from_slice(&h.params) {
                Ok(v)  => v,
                Err(e) => {
                    log::warn!("[cluster] hint #{} unparseable, dropping: {e}", h.seq);
                    succeeded.push(h.seq);  // delete it
                    continue;
                }
            };
            match replication::call_peer_v2(cluster, &url, &h.method, &params).await {
                Ok(_) => {
                    succeeded.push(h.seq);
                    log::debug!("[cluster] replayed hint #{} to {}", h.seq, url);
                }
                Err(e) => {
                    // Stop draining this peer — likely still down or
                    // overloaded; come back next tick.
                    log::debug!("[cluster] replay hint #{} to {} failed: {e}", h.seq, url);
                    break;
                }
            }
        }
        if !succeeded.is_empty() {
            if let Err(e) = cluster.hints.delete_seqs(&succeeded) {
                log::warn!("[cluster] delete_seqs: {e}");
            } else {
                total_replayed += succeeded.len() as u64;
                log::info!("[cluster] replayed {} hint(s) for peer {}", succeeded.len(), peer_id);
            }
        }
    }
    total_replayed
}

// ── Anti-entropy ─────────────────────────────────────────────────────────────

/// Per-tick anti-entropy outcome (sums across every replicated store).
#[derive(Debug, Default, Clone, Copy)]
pub struct AntientropyStats {
    pub pulled:     u64,
    pub tombstones: u64,
    pub pruned:     u64,
}

/// Run one anti-entropy tick: for each fully-replicated store
/// (`docs`, `signals`, `scripts`), pick a random Alive peer and pull any
/// live entries we're missing + apply any tombstones we don't yet have.
pub async fn antientropy_tick(cluster: &Arc<bdslib::Cluster>) -> AntientropyStats {
    let mut out = AntientropyStats::default();

    // Tombstone GC first — keep storage bounded regardless of peer state.
    if let Ok(dropped) = cluster.tombstones.prune_old(cluster.config.hint_max_age_secs.saturating_mul(2)) {
        if dropped > 0 {
            log::info!("[antientropy] pruned {dropped} expired tombstone(s)");
        }
        out.pruned = dropped;
    }

    let peer = match cluster.peers.read().pick_random_alive() {
        Some(p) => p,
        None    => return out,  // standalone — nothing to sync against
    };

    for &store in &["docs", "signals", "scripts", "users"] {
        if !cluster.config.full_replication_stores.iter().any(|s| s == store) {
            continue;
        }
        match sync_store(cluster, &peer, store).await {
            Ok((pulled, tombs)) => { out.pulled += pulled; out.tombstones += tombs; }
            Err(e) => log::warn!("[antientropy] {store} <- {}: {e}", peer.url),
        }
    }

    // Graph store — two id-spaces (nodes + edges) plus a fingerprint
    // pre-check, so it gets its own sync path rather than the generic
    // one-id-space `sync_store`.
    if cluster.config.full_replication_stores.iter().any(|s| s == "graph") {
        match sync_graph(cluster, &peer).await {
            Ok((pulled, tombs)) => { out.pulled += pulled; out.tombstones += tombs; }
            Err(e) => log::warn!("[antientropy] graph <- {}: {e}", peer.url),
        }
    }
    out
}

async fn sync_store(
    cluster:   &Arc<bdslib::Cluster>,
    peer:      &bdslib::cluster::Peer,
    store:     &str,
) -> Result<(u64, u64), String> {
    use bdslib::cluster::replication;

    // Two naming conventions co-exist:
    //   docs/signals/scripts/users → "v2/<singular>.list_ids"
    //   llm_cache                  → "v2/llm.cache.list_ids" (dotted family)
    let list_method = match store {
        "llm_cache" => "v2/llm.cache.list_ids".to_owned(),
        _ => format!("v2/{}.list_ids",
            match store {
                "scripts" => "script",
                "signals" => "signal",
                "users"   => "user",
                _         => "doc",
            }
        ),
    };

    // 1. Pull peer's view.
    let remote = replication::call_peer_v2(cluster, &peer.url, &list_method, &serde_json::json!({}))
        .await
        .map_err(|e| e.to_string())?;
    let remote_live: Vec<(uuid::Uuid, u64)> = remote.get("live").and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|item| {
            let id = uuid::Uuid::parse_str(item.get("id")?.as_str()?).ok()?;
            let ts = item.get("updated_at").and_then(|v| v.as_u64()).unwrap_or(0);
            Some((id, ts))
        }).collect())
        .unwrap_or_default();
    let remote_tombs: Vec<(uuid::Uuid, i64)> = remote.get("tombstones").and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|item| {
            let id = uuid::Uuid::parse_str(item.get("id")?.as_str()?).ok()?;
            let ts = item.get("deleted_at").and_then(|v| v.as_i64()).unwrap_or(0);
            Some((id, ts))
        }).collect())
        .unwrap_or_default();

    // 2. Build local-known sets via the same v2 helper (cheaper than
    // re-implementing it; one extra synchronous DuckDB query).
    let cluster_for_local = cluster.clone();
    let store_owned = store.to_owned();
    let local = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let db = bdslib::get_db().map_err(|e| e.to_string())?;
        let entries: Vec<(uuid::Uuid, serde_json::Value)> = match store_owned.as_str() {
            "docs"    => db.docstore_list_metadata().map_err(|e| e.to_string())?,
            "signals" => db.signals_list_metadata().map_err(|e| e.to_string())?,
            "scripts" => db.scripts_with_metadata().map_err(|e| e.to_string())?,
            "users"   => {
                // Users store has updated_at as a top-level column;
                // synthesise a {updated_at} metadata blob so the
                // downstream LWW comparator just works.
                let cluster_u = cluster_for_local.clone();
                cluster_u.users.list_summaries()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|s| (s.id, serde_json::json!({"updated_at": s.updated_at})))
                    .collect()
            }
            "llm_cache" => {
                // Inference cache has updated_at as a column too; the
                // global CacheManager is the source of truth.  Skip
                // entirely on nodes where the cache wasn't initialised
                // (returns an empty live set rather than erroring out).
                match bdslib::llm::cache::manager() {
                    Some(mgr) => mgr.cache().list_ids()
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .map(|(id, ts)| (id, serde_json::json!({"updated_at": ts})))
                        .collect(),
                    None => Vec::new(),
                }
            }
            _ => return Err(format!("unknown store {store_owned:?}")),
        };
        let live: Vec<serde_json::Value> = entries.into_iter().map(|(id, meta)| {
            let updated_at = meta.get("updated_at").and_then(|v| v.as_u64())
                .or_else(|| meta.get("timestamp").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            serde_json::json!({"id": id.to_string(), "updated_at": updated_at})
        }).collect();
        let tombs: Vec<serde_json::Value> = cluster_for_local.tombstones.list_for_store(&store_owned)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|t| serde_json::json!({"id": t.id.to_string(), "deleted_at": t.deleted_at}))
            .collect();
        Ok(serde_json::json!({"live": live, "tombstones": tombs}))
    })
    .await
    .map_err(|e| e.to_string())??;

    let local_live_ts: std::collections::HashMap<uuid::Uuid, u64> = local.get("live")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|i| {
            let id = uuid::Uuid::parse_str(i.get("id")?.as_str()?).ok()?;
            let ts = i.get("updated_at").and_then(|v| v.as_u64()).unwrap_or(0);
            Some((id, ts))
        }).collect())
        .unwrap_or_default();
    let local_tomb_ids: std::collections::HashSet<uuid::Uuid> = local.get("tombstones")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|i| {
            uuid::Uuid::parse_str(i.get("id")?.as_str()?).ok()
        }).collect())
        .unwrap_or_default();

    // 3. Apply remote tombstones we don't yet have (delete + tombstone locally).
    let mut applied_tombs = 0;
    for (id, deleted_at) in &remote_tombs {
        if !local_tomb_ids.contains(id) {
            let cluster_t = cluster.clone();
            let id_t = *id;
            let deleted_at_t = *deleted_at;
            let store_t = store.to_owned();
            let _ = tokio::task::spawn_blocking(move || -> Result<(), String> {
                let db = bdslib::get_db().map_err(|e| e.to_string())?;
                match store_t.as_str() {
                    "docs"    => { let _ = db.doc_delete(id_t); }
                    "scripts" => { let _ = db.script_delete(id_t); }
                    "users"   => {
                        // Users do get deleted via v3/user.delete +
                        // tombstone propagation; the AE pull path
                        // mirrors the same delete here so a peer that
                        // missed the original delete catches up.
                        if let Some(c) = db.cluster() {
                            let _ = c.users.delete(id_t);
                        }
                    }
                    "llm_cache" => {
                        if let Some(mgr) = bdslib::llm::cache::manager() {
                            let _ = mgr.cache().delete(id_t);
                        }
                    }
                    _         => {}  // signals don't get deleted in Phase 4
                }
                cluster_t.tombstones.mark_deleted(&store_t, id_t, deleted_at_t)
                    .map_err(|e| e.to_string())
            }).await;
            applied_tombs += 1;
        }
    }

    // 4. Pull entries we're missing (skip if already tombstoned locally),
    //    OR LWW-overwrite if remote has a newer updated_at than local.
    //    Phase 5: previously only the missing-on-local case was handled.
    let mut pulled = 0;
    for (id, remote_ts) in &remote_live {
        if local_tomb_ids.contains(id) { continue; }
        match local_live_ts.get(id) {
            None => {
                // Missing locally → fresh pull (add).
                if let Err(e) = pull_one(cluster, peer, store, *id).await {
                    log::debug!("[antientropy] pull {store}/{id} from {}: {e}", peer.url);
                    continue;
                }
                pulled += 1;
            }
            Some(&local_ts) if *remote_ts > local_ts => {
                // Present on both sides, remote is newer → LWW overwrite.
                if let Err(e) = overwrite_one(cluster, peer, store, *id).await {
                    log::debug!("[antientropy] LWW overwrite {store}/{id} from {}: {e}", peer.url);
                    continue;
                }
                pulled += 1;
            }
            _ => {}  // local is at least as fresh — leave alone
        }
    }

    if pulled > 0 || applied_tombs > 0 {
        log::info!("[antientropy] {store} <- {}: pulled={pulled} tombstones={applied_tombs}", peer.url);
    }
    Ok((pulled as u64, applied_tombs as u64))
}

async fn pull_one(
    cluster: &Arc<bdslib::Cluster>,
    peer:    &bdslib::cluster::Peer,
    store:   &str,
    id:      uuid::Uuid,
) -> Result<(), String> {
    use bdslib::cluster::replication;

    match store {
        "docs" => {
            let meta = replication::call_peer_v2(cluster, &peer.url, "v2/doc.get.metadata",
                &serde_json::json!({ "session": "antientropy", "id": id.to_string() })).await
                .map_err(|e| e.to_string())?;
            let metadata = meta.get("metadata").cloned().unwrap_or_default();

            let cont = replication::call_peer_v2(cluster, &peer.url, "v2/doc.get.content",
                &serde_json::json!({ "session": "antientropy", "id": id.to_string() })).await
                .map_err(|e| e.to_string())?;
            let content = cont.get("content").and_then(|v| v.as_str()).unwrap_or("").to_owned();

            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let db = bdslib::get_db().map_err(|e| e.to_string())?;
                if db.doc_get_metadata(id).map_err(|e| e.to_string())?.is_some() {
                    return Ok(());  // raced: someone else added it
                }
                db.doc_add_with_id(id, metadata, content.as_bytes()).map_err(|e| e.to_string())
            }).await.map_err(|e| e.to_string())?
        }
        "signals" => {
            // Phase 5: pull missing signal via the dedicated v2/signal.get
            // endpoint, then emit locally with the same UUID.
            let resp = replication::call_peer_v2(cluster, &peer.url, "v2/signal.get",
                &serde_json::json!({ "session": "antientropy", "id": id.to_string() })).await
                .map_err(|e| e.to_string())?;
            let metadata = match resp.get("metadata") {
                Some(m) if !m.is_null() => m.clone(),
                _ => return Ok(()),  // remote no longer has it (race) — nothing to do
            };
            // Decompose into the fields signal_emit_with_id wants.
            let name      = metadata.get("name").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let severity  = metadata.get("severity").and_then(|v| v.as_str()).unwrap_or("info").to_owned();
            let timestamp = metadata.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
            // Strip the three required fields from the extra map; the
            // helper re-injects them.
            let mut extra = match metadata {
                serde_json::Value::Object(m) => m,
                _ => serde_json::Map::new(),
            };
            extra.remove("name");
            extra.remove("severity");
            extra.remove("timestamp");

            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let db = bdslib::get_db().map_err(|e| e.to_string())?;
                if db.signal_get(id).map_err(|e| e.to_string())?.is_some() {
                    return Ok(());  // raced — already pulled
                }
                db.signal_emit_with_id(id, &name, &severity, timestamp, extra)
                    .map_err(|e| e.to_string())
            }).await.map_err(|e| e.to_string())?
        }
        "scripts" => {
            let meta = replication::call_peer_v2(cluster, &peer.url, "v2/script",
                &serde_json::json!({ "session": "antientropy", "id": id.to_string() })).await
                .map_err(|e| e.to_string())?;
            let metadata = meta.get("metadata").cloned().unwrap_or_default();
            let body     = meta.get("script").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let db = bdslib::get_db().map_err(|e| e.to_string())?;
                if db.script_metadata(id).map_err(|e| e.to_string())?.is_some() {
                    return Ok(());
                }
                db.script_add_with_id(id, metadata, &body).map_err(|e| e.to_string())
            }).await.map_err(|e| e.to_string())?
        }
        "users" => {
            use bdslib::cluster::credential::AuthMethod;
            // Fetch the peer's full row (including credential_hash) by id.
            let resp = replication::call_peer_v2(cluster, &peer.url, "v2/user.get_by_id",
                &serde_json::json!({ "id": id.to_string() })).await
                .map_err(|e| e.to_string())?;
            let user = match resp.get("user").filter(|v| !v.is_null()) {
                Some(u) => u.clone(),
                None    => return Ok(()),  // race: peer no longer has it
            };
            let username        = user.get("username").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let credential_hash = user.get("credential_hash").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let method_s        = user.get("auth_method").and_then(|v| v.as_str()).unwrap_or("password").to_owned();
            let metadata        = user.get("metadata").cloned().unwrap_or_default();
            let created_at      = user.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0);
            let updated_at      = user.get("updated_at").and_then(|v| v.as_u64()).unwrap_or(0);
            let disabled        = user.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false);

            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let db = bdslib::get_db().map_err(|e| e.to_string())?;
                let cluster = db.cluster()
                    .ok_or_else(|| "cluster mode disabled".to_owned())?;
                cluster.users.add_with_hash(
                    id, &username, &credential_hash,
                    AuthMethod::from_wire(&method_s),
                    metadata, created_at, updated_at, disabled,
                ).map_err(|e| e.to_string())
            }).await.map_err(|e| e.to_string())?
        }
        "llm_cache" => {
            // Phase 3.c: pull a single inference-cache row by id from `peer`
            // and apply it locally.  Each row is independent — no metadata
            // / content split like docs.
            let resp = replication::call_peer_v2(cluster, &peer.url, "v2/llm.cache.get.by_id",
                &serde_json::json!({ "id": id.to_string() })).await
                .map_err(|e| e.to_string())?;
            if !resp.get("found").and_then(|v| v.as_bool()).unwrap_or(false) {
                return Ok(());  // race: peer no longer has it
            }
            let cache_key     = resp.get("cache_key").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let provider      = resp.get("provider").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let model         = resp.get("model").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let kind          = resp.get("kind").and_then(|v| v.as_str()).unwrap_or("complete").to_owned();
            let request_json  = resp.get("request_json").cloned().unwrap_or(serde_json::Value::Null);
            let response_json = resp.get("response_json").cloned().unwrap_or(serde_json::Value::Null);
            let source_meta   = resp.get("source_meta").cloned().filter(|v| !v.is_null());
            let created_at    = resp.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0);
            let expires_at    = resp.get("expires_at").and_then(|v| v.as_u64()).unwrap_or(0);

            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let mgr = bdslib::llm::cache::manager()
                    .ok_or_else(|| "llm cache not initialised on this node".to_owned())?;
                let insert = bdslib::llm::cache::CacheInsert {
                    id, cache_key, provider, model, kind,
                    request_json, response_json, source_meta,
                    created_at, expires_at,
                };
                mgr.cache().put(insert).map_err(|e| e.to_string())
            }).await.map_err(|e| e.to_string())?
        }
        _ => Ok(())
    }
}

/// Phase 5 LWW pull: the entry already exists locally but the remote has a
/// newer `metadata.updated_at`.  Overwrite the local copy.  Signals don't
/// have updates, so this is a no-op for the signals store.
async fn overwrite_one(
    cluster: &Arc<bdslib::Cluster>,
    peer:    &bdslib::cluster::Peer,
    store:   &str,
    id:      uuid::Uuid,
) -> Result<(), String> {
    use bdslib::cluster::replication;
    match store {
        "docs" => {
            let meta = replication::call_peer_v2(cluster, &peer.url, "v2/doc.get.metadata",
                &serde_json::json!({ "session": "antientropy", "id": id.to_string() })).await
                .map_err(|e| e.to_string())?;
            let metadata = meta.get("metadata").cloned().unwrap_or_default();

            let cont = replication::call_peer_v2(cluster, &peer.url, "v2/doc.get.content",
                &serde_json::json!({ "session": "antientropy", "id": id.to_string() })).await
                .map_err(|e| e.to_string())?;
            let content = cont.get("content").and_then(|v| v.as_str()).unwrap_or("").to_owned();

            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let db = bdslib::get_db().map_err(|e| e.to_string())?;
                db.doc_update_metadata(id, metadata).map_err(|e| e.to_string())?;
                db.doc_update_content(id, content.as_bytes()).map_err(|e| e.to_string())
            }).await.map_err(|e| e.to_string())?
        }
        "scripts" => {
            let meta = replication::call_peer_v2(cluster, &peer.url, "v2/script",
                &serde_json::json!({ "session": "antientropy", "id": id.to_string() })).await
                .map_err(|e| e.to_string())?;
            let metadata = meta.get("metadata").cloned().unwrap_or_default();
            let body     = meta.get("script").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let db = bdslib::get_db().map_err(|e| e.to_string())?;
                db.update_script(id, metadata, &body).map_err(|e| e.to_string())
            }).await.map_err(|e| e.to_string())?
        }
        "users" => {
            use bdslib::cluster::credential::AuthMethod;
            // Pull the peer's full row by id, then replace ours
            // verbatim.  The remote `updated_at > local.updated_at`
            // check that gated this call already happened in
            // `sync_store`, so unconditional overwrite is correct.
            let resp = replication::call_peer_v2(cluster, &peer.url, "v2/user.get_by_id",
                &serde_json::json!({ "id": id.to_string() })).await
                .map_err(|e| e.to_string())?;
            let user = match resp.get("user").filter(|v| !v.is_null()) {
                Some(u) => u.clone(),
                None    => return Ok(()),
            };
            let username        = user.get("username").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let credential_hash = user.get("credential_hash").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let method_s        = user.get("auth_method").and_then(|v| v.as_str()).unwrap_or("password").to_owned();
            let metadata        = user.get("metadata").cloned().unwrap_or_default();
            let created_at      = user.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0);
            let updated_at      = user.get("updated_at").and_then(|v| v.as_u64()).unwrap_or(0);
            let disabled        = user.get("disabled").and_then(|v| v.as_bool()).unwrap_or(false);

            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let db = bdslib::get_db().map_err(|e| e.to_string())?;
                let cluster = db.cluster()
                    .ok_or_else(|| "cluster mode disabled".to_owned())?;
                // Drop the old row, write the new one verbatim.  Both
                // operations are idempotent so a partial failure on
                // re-pull next tick converges.
                cluster.users.delete(id).map_err(|e| e.to_string())?;
                cluster.users.add_with_hash(
                    id, &username, &credential_hash,
                    AuthMethod::from_wire(&method_s),
                    metadata, created_at, updated_at, disabled,
                ).map_err(|e| e.to_string())
            }).await.map_err(|e| e.to_string())?
        }
        // Signals are append-only — no updates exist.
        _ => Ok(()),
    }
}

// ── graph anti-entropy ───────────────────────────────────────────────────────

/// Parse a `Node` from the JSON shape `v2/graph.node.get` returns.
fn json_to_graph_node(v: &serde_json::Value) -> Option<bdslib::graphstorage::Node> {
    Some(bdslib::graphstorage::Node {
        id:         uuid::Uuid::parse_str(v.get("id")?.as_str()?).ok()?,
        node_type:  v.get("node_type")?.as_str()?.to_owned(),
        ref_id:     v.get("ref_id")?.as_str()?.to_owned(),
        attrs:      v.get("attrs").cloned().unwrap_or_else(|| serde_json::json!({})),
        created_at: v.get("created_at").and_then(|x| x.as_i64()).unwrap_or(0),
        updated_at: v.get("updated_at").and_then(|x| x.as_i64()).unwrap_or(0),
    })
}

/// Parse an `Edge` from the JSON shape `v2/graph.edge.get` returns.
fn json_to_graph_edge(v: &serde_json::Value) -> Option<bdslib::graphstorage::Edge> {
    Some(bdslib::graphstorage::Edge {
        id:         uuid::Uuid::parse_str(v.get("id")?.as_str()?).ok()?,
        src:        uuid::Uuid::parse_str(v.get("src")?.as_str()?).ok()?,
        dst:        uuid::Uuid::parse_str(v.get("dst")?.as_str()?).ok()?,
        edge_type:  v.get("edge_type")?.as_str()?.to_owned(),
        weight:     v.get("weight").and_then(|x| x.as_f64()).unwrap_or(1.0),
        directed:   v.get("directed").and_then(|x| x.as_bool()).unwrap_or(true),
        attrs:      v.get("attrs").cloned().unwrap_or_else(|| serde_json::json!({})),
        valid_from: v.get("valid_from").and_then(|x| x.as_i64()).unwrap_or(0),
        valid_to:   v.get("valid_to").and_then(|x| x.as_i64()).unwrap_or(0),
        created_at: v.get("created_at").and_then(|x| x.as_i64()).unwrap_or(0),
        updated_at: v.get("updated_at").and_then(|x| x.as_i64()).unwrap_or(0),
    })
}

/// Anti-entropy for the fully-replicated graph store.
///
/// Step 1 — compare cheap whole-store **fingerprints**; identical means
/// already converged with this peer, so we skip the diff entirely.
/// Step 2 — pull the peer's node + edge enumeration (`v2/graph.list_ids`)
/// and our own; the entity ids are deterministic, so the diff is a pure
/// id-set + LWW-`updated_at` comparison across two id-spaces.
/// Step 3 — apply the peer's tombstones we are missing (delete + record).
/// Step 4 — pull every node, then every edge, that is missing locally or
/// older than the peer's copy, and apply it through the cache-/FTS-
/// coherent `apply_*_lww` primitives.  Nodes before edges keeps
/// referential integrity.
///
/// Returns `(entities_pulled, tombstones_applied)`.
async fn sync_graph(
    cluster: &Arc<bdslib::Cluster>,
    peer:    &bdslib::cluster::Peer,
) -> Result<(u64, u64), String> {
    use std::collections::{HashMap, HashSet};

    // ── 1. Fingerprint pre-check ──────────────────────────────────────────────
    let remote_fp = replication::call_peer_v2(cluster, &peer.url, "v2/graph.fingerprint",
        &serde_json::json!({})).await.map_err(|e| e.to_string())?;
    let local_fp = tokio::task::spawn_blocking(|| -> Result<serde_json::Value, String> {
        let db = bdslib::get_db().map_err(|e| e.to_string())?;
        let f = db.graph_fingerprint().map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "nodes_hash": f.nodes_hash, "edges_hash": f.edges_hash }))
    }).await.map_err(|e| e.to_string())??;
    if remote_fp.get("nodes_hash") == local_fp.get("nodes_hash")
        && remote_fp.get("edges_hash") == local_fp.get("edges_hash")
    {
        return Ok((0, 0)); // already converged with this peer
    }

    // ── 2. Enumerate both sides ───────────────────────────────────────────────
    let remote = replication::call_peer_v2(cluster, &peer.url, "v2/graph.list_ids",
        &serde_json::json!({})).await.map_err(|e| e.to_string())?;
    let local = tokio::task::spawn_blocking(|| -> Result<serde_json::Value, String> {
        let db = bdslib::get_db().map_err(|e| e.to_string())?;
        let nodes = db.graph_node_summaries().map_err(|e| e.to_string())?;
        let edges = db.graph_edge_summaries().map_err(|e| e.to_string())?;
        let (ntomb, etomb) = match db.cluster() {
            Some(c) => (
                c.tombstones.list_for_store("graph_nodes").map_err(|e| e.to_string())?,
                c.tombstones.list_for_store("graph_edges").map_err(|e| e.to_string())?,
            ),
            None => (Vec::new(), Vec::new()),
        };
        Ok(serde_json::json!({
            "nodes": nodes.iter()
                .map(|n| serde_json::json!({ "id": n.id.to_string(), "updated_at": n.updated_at }))
                .collect::<Vec<_>>(),
            "edges": edges.iter()
                .map(|e| serde_json::json!({ "id": e.id.to_string(), "updated_at": e.updated_at }))
                .collect::<Vec<_>>(),
            "node_tombstones": ntomb.iter().map(|t| t.id.to_string()).collect::<Vec<_>>(),
            "edge_tombstones": etomb.iter().map(|t| t.id.to_string()).collect::<Vec<_>>(),
        }))
    }).await.map_err(|e| e.to_string())??;

    let map_of = |v: &serde_json::Value, key: &str| -> HashMap<String, i64> {
        v.get(key).and_then(|a| a.as_array()).map(|arr| arr.iter().filter_map(|i| {
            Some((
                i.get("id")?.as_str()?.to_owned(),
                i.get("updated_at").and_then(|x| x.as_i64()).unwrap_or(0),
            ))
        }).collect()).unwrap_or_default()
    };
    let set_of = |v: &serde_json::Value, key: &str| -> HashSet<String> {
        v.get(key).and_then(|a| a.as_array()).map(|arr| arr.iter()
            .filter_map(|i| i.as_str().map(str::to_owned)).collect()).unwrap_or_default()
    };
    let local_nodes      = map_of(&local, "nodes");
    let local_edges      = map_of(&local, "edges");
    let local_node_tombs = set_of(&local, "node_tombstones");
    let local_edge_tombs = set_of(&local, "edge_tombstones");

    let mut pulled = 0u64;
    let mut tombs_applied = 0u64;
    let empty: Vec<serde_json::Value> = Vec::new();

    // ── 3. Apply remote tombstones we are missing ─────────────────────────────
    for t in remote.get("node_tombstones").and_then(|v| v.as_array()).unwrap_or(&empty) {
        let id = match t.get("id").and_then(|v| v.as_str()) { Some(s) => s.to_owned(), None => continue };
        if local_node_tombs.contains(&id) { continue; }
        let id_u = match uuid::Uuid::parse_str(&id) { Ok(u) => u, Err(_) => continue };
        let deleted_at = t.get("deleted_at").and_then(|v| v.as_i64()).unwrap_or(0);
        let cluster_t = cluster.clone();
        let _ = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let db = bdslib::get_db().map_err(|e| e.to_string())?;
            if let Some(node) = db.graph_get_node_by_id(&id_u).map_err(|e| e.to_string())? {
                let _ = db.graph_remove_node(&node.node_ref());
            }
            cluster_t.tombstones.mark_deleted("graph_nodes", id_u, deleted_at)
                .map_err(|e| e.to_string())
        }).await;
        tombs_applied += 1;
    }
    for t in remote.get("edge_tombstones").and_then(|v| v.as_array()).unwrap_or(&empty) {
        let id = match t.get("id").and_then(|v| v.as_str()) { Some(s) => s.to_owned(), None => continue };
        if local_edge_tombs.contains(&id) { continue; }
        let id_u = match uuid::Uuid::parse_str(&id) { Ok(u) => u, Err(_) => continue };
        let deleted_at = t.get("deleted_at").and_then(|v| v.as_i64()).unwrap_or(0);
        let cluster_t = cluster.clone();
        let _ = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let db = bdslib::get_db().map_err(|e| e.to_string())?;
            let _ = db.graph_delete_edge(&id_u);
            cluster_t.tombstones.mark_deleted("graph_edges", id_u, deleted_at)
                .map_err(|e| e.to_string())
        }).await;
        tombs_applied += 1;
    }

    // ── 4a. Pull missing / newer nodes ────────────────────────────────────────
    for n in remote.get("nodes").and_then(|v| v.as_array()).unwrap_or(&empty) {
        let id = match n.get("id").and_then(|v| v.as_str()) { Some(s) => s.to_owned(), None => continue };
        if local_node_tombs.contains(&id) { continue; } // locally deleted — don't resurrect
        let remote_ts = n.get("updated_at").and_then(|v| v.as_i64()).unwrap_or(0);
        let need = match local_nodes.get(&id) { None => true, Some(&lt) => remote_ts > lt };
        if !need { continue; }
        let resp = match replication::call_peer_v2(cluster, &peer.url, "v2/graph.node.get",
            &serde_json::json!({ "id": id })).await
        {
            Ok(r) => r,
            Err(e) => { log::debug!("[antientropy] graph node.get {id}: {e}"); continue; }
        };
        let Some(node_json) = resp.get("node").filter(|v| !v.is_null()).cloned() else { continue };
        let _ = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let db = bdslib::get_db().map_err(|e| e.to_string())?;
            if let Some(node) = json_to_graph_node(&node_json) {
                db.graph_apply_node_lww(&node).map_err(|e| e.to_string())?;
            }
            Ok(())
        }).await;
        pulled += 1;
    }

    // ── 4b. Pull missing / newer edges (after nodes — referential order) ──────
    for e in remote.get("edges").and_then(|v| v.as_array()).unwrap_or(&empty) {
        let id = match e.get("id").and_then(|v| v.as_str()) { Some(s) => s.to_owned(), None => continue };
        if local_edge_tombs.contains(&id) { continue; }
        let remote_ts = e.get("updated_at").and_then(|v| v.as_i64()).unwrap_or(0);
        let need = match local_edges.get(&id) { None => true, Some(&lt) => remote_ts > lt };
        if !need { continue; }
        let resp = match replication::call_peer_v2(cluster, &peer.url, "v2/graph.edge.get",
            &serde_json::json!({ "id": id })).await
        {
            Ok(r) => r,
            Err(e) => { log::debug!("[antientropy] graph edge.get {id}: {e}"); continue; }
        };
        let Some(edge_json) = resp.get("edge").filter(|v| !v.is_null()).cloned() else { continue };
        let _ = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let db = bdslib::get_db().map_err(|e| e.to_string())?;
            if let Some(edge) = json_to_graph_edge(&edge_json) {
                db.graph_apply_edge_lww(&edge).map_err(|e| e.to_string())?;
            }
            Ok(())
        }).await;
        pulled += 1;
    }

    if pulled > 0 || tombs_applied > 0 {
        log::info!("[antientropy] graph <- {}: pulled={pulled} tombstones={tombs_applied}", peer.url);
    }
    Ok((pulled, tombs_applied))
}
