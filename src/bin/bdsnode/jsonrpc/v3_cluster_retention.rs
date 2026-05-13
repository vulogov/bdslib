//! `v3/cluster.retention.status` — cluster-wide retention introspection.
//!
//! Read-only fan-out: calls `v2/retention.settings` on every Alive
//! peer in parallel, plus the local handler in-process, and returns
//! a summary block that surfaces policy drift between peers.
//!
//! **Phase 2 design note**: this RPC is the operator's eye-in-the-sky
//! for retention.  It does NOT trigger sweeps across the cluster (that
//! would be too easy to misuse — `--force --duration 1s` on every
//! peer at once is permanent data loss).  Per-node sweeps stay under
//! the operator's deliberate control via `v2/retention.sweep`.

use super::params::{rpc_err, v3_cluster_meta};
use bdslib::cluster::fanout;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::{json, Value as JsonValue};
use std::collections::BTreeSet;

pub fn register(module: &mut RpcModule<()>) {
    module.register_async_method("v3/cluster.retention.status", |_params, _ctx, _| async move {
        log::debug!("v3/cluster.retention.status: start");

        // Local view — read the OnceLock the bdsnode startup published
        // plus the lifetime stats counters.  Wrapped in spawn_blocking
        // out of paranoia (the read itself is just atomic loads, but
        // matching v3/* convention keeps the wiring simple if we ever
        // add catalog reads here).
        let local_fut = tokio::task::spawn_blocking(local_status);

        // Fan-out to peers in parallel.  Standalone (no cluster) →
        // empty result, no peers queried, no meta to merge.
        let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
        let fanout_fut = async {
            match &cluster {
                Some(c) => Some(fanout::fan_out_v2(c, "v2/retention.settings", json!({})).await),
                None    => None,
            }
        };

        let (local_res, fan) = tokio::join!(local_fut, fanout_fut);
        let local = local_res
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

        // Build the per-peer array.  Failed peers get an `error` field
        // instead of `settings` so the client can render a partial
        // table without dropping the row entirely.
        let peers: Vec<JsonValue> = fan.as_ref()
            .map(|f| f.responses.iter().map(|r| {
                let mut entry = json!({
                    "node_id": r.peer.node_id.to_string(),
                    "url":     r.peer.url,
                });
                match &r.result {
                    Ok(settings) => {
                        entry["settings"] = settings.clone();
                    }
                    Err(e) => {
                        entry["error"] = json!(e.to_string());
                    }
                }
                entry
            }).collect())
            .unwrap_or_default();

        let summary = build_summary(&local, &peers);

        log::debug!("v3/cluster.retention.status: done (peers={})", peers.len());
        Ok::<JsonValue, ErrorObject>(json!({
            "local":        local,
            "peers":        peers,
            "summary":      summary,
            "cluster_meta": v3_cluster_meta(fan),
        }))
    }).unwrap();
}

/// Local "this node" view — node identity + the same payload
/// `v2/retention.settings` would return.
///
/// Built from process-wide state directly so the local handler can't
/// drift from what fan-out returns from peers calling the same RPC.
fn local_status() -> Result<JsonValue, ErrorObject<'static>> {
    use std::sync::atomic::Ordering;

    // Cluster identity — None when running standalone (no cluster
    // block in bds.hjson).  The summary still works without it.
    let (node_id, bind_url) = match bdslib::get_db().ok().and_then(|d| d.cluster().cloned()) {
        Some(c) => (Some(c.node_id.to_string()), Some(c.config.bind_url.clone())),
        None    => (None, None),
    };

    let s = bdslib::retention::stats();
    let stats_block = json!({
        "evicted_lifetime":         s.evicted_lifetime.load(Ordering::Relaxed),
        "evicted_last_run":         s.evicted_last_run.load(Ordering::Relaxed),
        "freed_lifetime_bytes":     s.freed_lifetime_bytes.load(Ordering::Relaxed),
        "freed_last_run_bytes":     s.freed_last_run_bytes.load(Ordering::Relaxed),
        "last_run_ts":              s.last_run_ts.load(Ordering::Relaxed),
        "last_run_ms":              s.last_run_ms.load(Ordering::Relaxed),
        "errors_lifetime":          s.errors_lifetime.load(Ordering::Relaxed),
        "quorum_skipped_lifetime":  s.quorum_skipped_lifetime.load(Ordering::Relaxed),
        "quorum_skipped_last_run":  s.quorum_skipped_last_run.load(Ordering::Relaxed),
    });

    // ActiveConfig mirrors what v2/retention.settings emits.  None
    // when `server::retention::start` never ran in this process
    // (test binaries, partial inits).
    let settings = match crate::server::retention::active() {
        Some(a) => json!({
            "installed":              true,
            "enabled":                a.enabled,
            "duration":               humantime::format_duration(a.duration).to_string(),
            "duration_secs":          a.duration.as_secs(),
            "interval_secs":          a.interval_secs,
            "max_evictions_per_run":  a.max_evictions_per_run,
            "dry_run":                 a.dry_run,
            "reload_drain_after_evict": a.reload_drain_after_evict,
            "drain_load_duration":    a.drain_load_duration,
            "quorum_check_enabled":   a.quorum_check_enabled,
            "quorum_min_peers":       a.quorum_min_peers,
            "stats":                  stats_block,
        }),
        None => json!({
            "installed":             false,
            "stats":                 stats_block,
        }),
    };

    Ok(json!({
        "node_id":  node_id,
        "url":      bind_url,
        "settings": settings,
    }))
}

/// Roll local + per-peer settings into a one-screen audit summary so
/// operators can spot policy drift at a glance.
///
/// `consistent: false` is the operator-visible alarm bell — it means
/// the same shard could be evicted by one peer and kept by another,
/// which is technically allowed (per-node policy is by design) but
/// usually unintended.
fn build_summary(local: &JsonValue, peers: &[JsonValue]) -> JsonValue {
    // Collect each node's effective settings entry (local + every
    // successful peer).
    let mut all_settings: Vec<&JsonValue> = Vec::with_capacity(1 + peers.len());
    if let Some(s) = local.get("settings") {
        all_settings.push(s);
    }
    for p in peers {
        if let Some(s) = p.get("settings") {
            all_settings.push(s);
        }
    }

    let mut distinct_durations: BTreeSet<String> = BTreeSet::new();
    let mut distinct_intervals: BTreeSet<u64>    = BTreeSet::new();
    let mut any_disabled  = false;
    let mut any_dry_run   = false;
    let mut total_nodes   = 0usize;

    // Aggregate lifetime telemetry across every node that answered.
    let mut total_evicted_lifetime:     u64 = 0;
    let mut total_freed_lifetime_bytes: u64 = 0;
    let mut total_errors_lifetime:      u64 = 0;
    let mut max_last_run_ts:            u64 = 0;
    let mut peers_uninstalled:          u64 = 0;

    for s in &all_settings {
        total_nodes += 1;
        let installed = s.get("installed").and_then(|v| v.as_bool()).unwrap_or(false);
        if !installed {
            peers_uninstalled += 1;
        }
        if let Some(d) = s.get("duration").and_then(|v| v.as_str()) {
            distinct_durations.insert(d.to_owned());
        }
        if let Some(n) = s.get("interval_secs").and_then(|v| v.as_u64()) {
            distinct_intervals.insert(n);
        }
        if s.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            any_disabled = true;
        }
        if s.get("dry_run").and_then(|v| v.as_bool()) == Some(true) {
            any_dry_run = true;
        }
        if let Some(stats) = s.get("stats") {
            total_evicted_lifetime     += stats.get("evicted_lifetime")    .and_then(|v| v.as_u64()).unwrap_or(0);
            total_freed_lifetime_bytes += stats.get("freed_lifetime_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
            total_errors_lifetime      += stats.get("errors_lifetime")     .and_then(|v| v.as_u64()).unwrap_or(0);
            let lrt = stats.get("last_run_ts").and_then(|v| v.as_u64()).unwrap_or(0);
            if lrt > max_last_run_ts { max_last_run_ts = lrt; }
        }
    }

    // Consistency = exactly one distinct duration AND exactly one
    // distinct interval AND no node reports `installed=false`.  We
    // ignore `dry_run` and `enabled` differences — those are
    // operational toggles, not policy.
    let consistent = distinct_durations.len() <= 1
        && distinct_intervals.len() <= 1
        && peers_uninstalled == 0;

    json!({
        "total_nodes":                  total_nodes,
        "consistent":                   consistent,
        "distinct_durations":           distinct_durations.into_iter().collect::<Vec<_>>(),
        "distinct_interval_secs":       distinct_intervals.into_iter().collect::<Vec<_>>(),
        "any_disabled":                 any_disabled,
        "any_dry_run":                  any_dry_run,
        "peers_uninstalled":            peers_uninstalled,
        "evicted_lifetime_total":       total_evicted_lifetime,
        "freed_lifetime_bytes_total":   total_freed_lifetime_bytes,
        "errors_lifetime_total":        total_errors_lifetime,
        "max_last_run_ts":              max_last_run_ts,
    })
}
