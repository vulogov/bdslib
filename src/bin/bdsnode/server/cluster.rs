//! Background tokio task that drives the cluster gossip loop and the periodic
//! liveness sweep.  Disabled when `cluster.enabled = false` (or absent).
//!
//! Bootstrap pass runs once at startup against `cluster.bootstrap` plus every
//! peer in the persisted `peers.json`, then the tick loop runs until shutdown.

use bdslib::cluster::{gossip, replication};
use bdslib::cluster::peer_table::PeerState;
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

    // One-shot bootstrap.  Best-effort: failures are logged and we keep
    // ticking; gossip will reconcile when peers reappear.
    record_bootstrap(&cluster, gossip::bootstrap(&cluster, &http).await);

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

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                log::debug!("[cluster] shutdown signal — stopping gossip");
                break;
            }
            _ = tokio::time::sleep(interval) => {
                tick_no = tick_no.wrapping_add(1);

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

    for &store in &["docs", "signals", "scripts"] {
        if !cluster.config.full_replication_stores.iter().any(|s| s == store) {
            continue;
        }
        match sync_store(cluster, &peer, store).await {
            Ok((pulled, tombs)) => { out.pulled += pulled; out.tombstones += tombs; }
            Err(e) => log::warn!("[antientropy] {store} <- {}: {e}", peer.url),
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

    let list_method = format!("v2/{}.list_ids",
        if store == "scripts" { "script".to_owned() }
        else if store == "signals" { "signal".to_owned() }
        else { "doc".to_owned() }
    );

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
        // Signals are append-only — no updates exist.
        _ => Ok(()),
    }
}
