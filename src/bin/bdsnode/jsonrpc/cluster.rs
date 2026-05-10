//! v3/cluster.* — membership and discovery RPC.
//!
//! Every method requires a valid HMAC-SHA256 signature in the `_hmac` field
//! (computed by the caller over the params object with `_hmac` omitted).  The
//! shared secret comes from `cluster.shared_secret` in `bds.hjson`.

use bdslib::cluster::{hmac_auth, peer_table::{Peer, PeerState}};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use uuid::Uuid;

use super::params::rpc_err;

/// Strip the `_hmac` field from a params object, return the canonical
/// JSON bytes that were signed plus the supplied signature.
fn extract_hmac(params: &mut serde_json::Map<String, JsonValue>) -> Result<(Vec<u8>, String), ErrorObject<'static>> {
    let sig = params.remove("_hmac")
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or_else(|| rpc_err(-32098, "missing _hmac field"))?;
    let canonical = serde_json::to_vec(&JsonValue::Object(params.clone()))
        .map_err(|e| rpc_err(-32600, format!("re-serialize params: {e}")))?;
    Ok((canonical, sig))
}

/// Authenticate a v3/cluster.* request and return the deserialized inner
/// params object (without `_hmac`).
fn authenticate(params: jsonrpsee::types::Params) -> Result<serde_json::Map<String, JsonValue>, ErrorObject<'static>> {
    let value: JsonValue = params.parse()
        .map_err(|e| rpc_err(-32602, format!("invalid params: {e}")))?;
    let mut obj = match value {
        JsonValue::Object(m) => m,
        _ => return Err(rpc_err(-32602, "params must be a JSON object")),
    };
    let (canonical, sig) = extract_hmac(&mut obj)?;

    let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
    let cluster = db.cluster()
        .ok_or_else(|| rpc_err(-32097, "cluster mode disabled on this node"))?;

    if !hmac_auth::verify(&cluster.config.shared_secret, &canonical, &sig) {
        return Err(rpc_err(-32098, "auth failed"));
    }
    Ok(obj)
}

pub fn register(module: &mut RpcModule<()>) {
    register_hello(module);
    register_peers(module);
    register_ping(module);
    register_status(module);
    register_sync(module);
}

// ── v3/cluster.hello ──────────────────────────────────────────────────────────

fn register_hello(module: &mut RpcModule<()>) {
    module.register_async_method("v3/cluster.hello", |params, _ctx, _| async move {
        let obj = authenticate(params)?;

        // Parse the caller's identity payload; missing fields are tolerated
        // (we just won't display them in the dashboard) but `node_id` and
        // `bind_url` are required to add the caller to our peer table.
        let caller_id_str = obj.get("node_id").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing node_id"))?;
        let caller_id = Uuid::parse_str(caller_id_str)
            .map_err(|e| rpc_err(-32602, format!("invalid node_id: {e}")))?;
        let caller_url = obj.get("bind_url").and_then(|v| v.as_str())
            .ok_or_else(|| rpc_err(-32602, "missing bind_url"))?
            .to_owned();
        let caller_version = obj.get("version").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let caller_emb     = obj.get("embedding_model").and_then(|v| v.as_str()).map(str::to_owned);
        let caller_started = obj.get("started_at").and_then(|v| v.as_u64()).unwrap_or(0);

        let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
        let cluster = db.cluster()
            .ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;

        // Refuse to peer with a node running a different embedding model;
        // their HNSW dimensions will not match and federated vector queries
        // would silently return wrong results.
        if let Some(my_emb) = cluster.embedding_model.read().clone() {
            if let Some(their) = &caller_emb {
                if !their.is_empty() && their != &my_emb {
                    return Err(rpc_err(-32096,
                        format!("embedding model mismatch: ours={my_emb} theirs={their}")));
                }
            }
        }

        // Add the caller to our table.
        {
            let mut t = cluster.peers.write();
            let mut p = Peer::new(caller_id, caller_url);
            p.state           = PeerState::Alive;
            p.last_seen       = now_secs();
            p.version         = caller_version;
            p.embedding_model = caller_emb;
            p.started_at      = caller_started;
            t.upsert(p);
        }
        cluster.persist_peers_best_effort();

        // Echo back our identity and our peer view so the caller converges
        // in a single round-trip.
        let snap = cluster.peers.read().snapshot();
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "node_id":         cluster.node_id.to_string(),
            "bind_url":        cluster.config.bind_url,
            "version":         env!("CARGO_PKG_VERSION"),
            "embedding_model": cluster.embedding_model.read().clone(),
            "started_at":      cluster.started_at_unix(),
            "peers":           snap,
        }))
    }).unwrap();
}

// ── v3/cluster.peers ──────────────────────────────────────────────────────────

fn register_peers(module: &mut RpcModule<()>) {
    module.register_async_method("v3/cluster.peers", |params, _ctx, _| async move {
        let _ = authenticate(params)?;
        let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
        let cluster = db.cluster()
            .ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
        let snap = cluster.peers.read().snapshot();
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "node_id": cluster.node_id.to_string(),
            "peers":   snap,
        }))
    }).unwrap();
}

// ── v3/cluster.ping ───────────────────────────────────────────────────────────

fn register_ping(module: &mut RpcModule<()>) {
    module.register_async_method("v3/cluster.ping", |params, _ctx, _| async move {
        let _ = authenticate(params)?;
        let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
        let cluster = db.cluster()
            .ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;
        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "node_id": cluster.node_id.to_string(),
            "ts":      now_secs(),
        }))
    }).unwrap();
}

// ── v3/cluster.status ─────────────────────────────────────────────────────────

fn register_status(module: &mut RpcModule<()>) {
    module.register_async_method("v3/cluster.status", |params, _ctx, _| async move {
        let _ = authenticate(params)?;
        let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
        let cluster = db.cluster()
            .ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?;

        let (alive, suspect, dead) = cluster.peers.read().count_by_state();
        let mode = cluster.mode();
        let stats = cluster.stats.read().clone();

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "node_id":            cluster.node_id.to_string(),
            "bind_url":           cluster.config.bind_url,
            "uptime_secs":        cluster.uptime().as_secs(),
            "mode":               mode.as_str(),
            "alive":              alive,
            "suspect":            suspect,
            "dead":               dead,
            "full_mode_threshold": cluster.config.full_mode_threshold,
            "replication_factor": cluster.config.replication_factor,
            "embedding_model":    cluster.embedding_model.read().clone(),
            "hint_backlog":       cluster.hints.len().unwrap_or(0),
            "tombstone_total":    cluster.tombstones.len().unwrap_or(0),
            "bootstrap_mode":     if cluster.config.floating_bootstrap { "floating" } else { "strict" },
            "stats": serde_json::json!({
                "last_hint_tick":           stats.last_hint_tick,
                "last_hint_tick_replayed":  stats.last_hint_tick_replayed,
                "last_ae_tick":             stats.last_ae_tick,
                "last_ae_tick_pulled":      stats.last_ae_tick_pulled,
                "last_ae_tick_tombstones":  stats.last_ae_tick_tombstones,
                "last_ae_tick_pruned":      stats.last_ae_tick_pruned,
                "last_bootstrap_attempt":   stats.last_bootstrap_attempt,
                "last_bootstrap_success":   stats.last_bootstrap_success,
                "last_bootstrap_attempted": stats.last_bootstrap_attempted,
                "last_bootstrap_joined":    stats.last_bootstrap_joined,
            }),
        }))
    }).unwrap();
}

// ── v3/cluster.sync ───────────────────────────────────────────────────────────

fn register_sync(module: &mut RpcModule<()>) {
    module.register_async_method("v3/cluster.sync", |params, _ctx, _| async move {
        let _ = authenticate(params)?;
        let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
        let cluster = db.cluster()
            .ok_or_else(|| rpc_err(-32097, "cluster mode disabled"))?
            .clone();

        // Run hint replay + AE on the spot.  Callers usually invoke this
        // after recovering from a network event ("I just brought node N
        // back, force the catch-up now").
        let replayed = crate::server::cluster::replay_hints(&cluster).await;
        let ae       = crate::server::cluster::antientropy_tick(&cluster).await;

        // Update the same telemetry fields the periodic loop writes.
        {
            let mut s = cluster.stats.write();
            s.last_hint_tick           = now_secs();
            s.last_hint_tick_replayed  = replayed;
            s.last_ae_tick             = now_secs();
            s.last_ae_tick_pulled      = ae.pulled;
            s.last_ae_tick_tombstones  = ae.tombstones;
            s.last_ae_tick_pruned      = ae.pruned;
        }

        Ok::<JsonValue, ErrorObject>(serde_json::json!({
            "node_id":          cluster.node_id.to_string(),
            "hints_replayed":   replayed,
            "ae_pulled":        ae.pulled,
            "ae_tombstones":    ae.tombstones,
            "ae_pruned":        ae.pruned,
            "hint_backlog":     cluster.hints.len().unwrap_or(0),
            "tombstone_total":  cluster.tombstones.len().unwrap_or(0),
        }))
    }).unwrap();
}

fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
