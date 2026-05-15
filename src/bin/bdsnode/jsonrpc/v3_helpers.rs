//! Shared helpers for v3/* analytics endpoints.
//!
//! `gather_cluster_fingerprints` is the workhorse: it fetches local
//! `(uuid, fingerprint)` pairs, fans out `v2/fingerprints.recent` to every
//! Alive peer, dedups by UUID (first-seen wins), and returns the merged
//! fingerprint vector ready to feed into `knn_summary_with`,
//! `ngram_anomaly_with`, or `ngram_remove_noise_with`.

use super::params::rpc_err;
use bdslib::cluster::fanout::{self, FanOutResults};
use jsonrpsee::types::ErrorObject;
use std::collections::HashSet;
use uuid::Uuid;

/// Result of a cluster-wide fingerprint sweep.
pub struct ClusterFingerprints {
    /// Deduplicated fingerprint strings.  Order: local first, then peer-by-peer
    /// in fan-out completion order.  Entries are guaranteed unique by UUID.
    pub fingerprints: Vec<String>,
    /// Total UUIDs seen before dedup (sum of local + per-peer counts).
    pub raw_total: usize,
    /// `None` means cluster mode is disabled on this node — only local
    /// fingerprints were used.
    pub fan: Option<FanOutResults>,
}

/// Fetch and dedup fingerprints across the local node and every Alive peer.
pub async fn gather_cluster_fingerprints(
    duration_str: &str,
) -> Result<ClusterFingerprints, ErrorObject<'static>> {
    let dur = humantime::parse_duration(duration_str)
        .map_err(|e| rpc_err(-32600, format!("invalid duration {duration_str:?}: {e}")))?;

    // Local pairs on a blocking thread.
    let local_fut = tokio::task::spawn_blocking(move || -> Result<Vec<(Uuid, String)>, ErrorObject<'static>> {
        let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
        db.fingerprints_with_ids_in_recent(dur)
            .map_err(|e| rpc_err(-32004, e))
    });

    // Concurrently fan out to peers.  We forward `duration` verbatim.
    let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
    let params  = serde_json::json!({ "duration": duration_str });
    let fanout_fut = async {
        match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/fingerprints.recent", params).await),
            None    => None,
        }
    };

    let (local_res, fan) = tokio::join!(local_fut, fanout_fut);
    let local_pairs = local_res
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

    // Dedup by UUID, first-seen wins.  Local goes in first so it always
    // takes precedence (typically the cheapest source).
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut raw_total = local_pairs.len();
    for (id, fp) in local_pairs {
        if seen.insert(id) { out.push(fp); }
    }
    if let Some(f) = &fan {
        for body in f.ok_results() {
            let arr = match body.get("fingerprints").and_then(|v| v.as_array()) {
                Some(a) => a,
                None    => continue,
            };
            raw_total += arr.len();
            for item in arr {
                let id_str = match item.get("id").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None    => continue,
                };
                let id = match Uuid::parse_str(id_str) {
                    Ok(u)  => u,
                    Err(_) => continue,
                };
                if !seen.insert(id) { continue; }  // duplicate replica
                if let Some(fp) = item.get("fingerprint").and_then(|v| v.as_str()) {
                    if !fp.trim().is_empty() {
                        out.push(fp.to_owned());
                    }
                }
            }
        }
    }

    Ok(ClusterFingerprints { fingerprints: out, raw_total, fan })
}

// ── Timed variant (Markov projection) ────────────────────────────────────────
//
// Same shape as `ClusterFingerprints` but every entry carries its
// observed Unix timestamp.  Drives `v3/project_logs`, which needs the
// inter-arrival distribution to feed `markov_project_timed_with`.

pub struct ClusterFingerprintsTimed {
    /// Deduped `(timestamp, fingerprint)` pairs, sorted ascending by
    /// timestamp so the downstream Markov projection observes events
    /// in chronological order across the cluster.
    pub events:    Vec<(i64, String)>,
    pub raw_total: usize,
    pub fan:       Option<FanOutResults>,
}

pub async fn gather_cluster_fingerprints_timed(
    duration_str: &str,
) -> Result<ClusterFingerprintsTimed, ErrorObject<'static>> {
    let dur = humantime::parse_duration(duration_str)
        .map_err(|e| rpc_err(-32600, format!("invalid duration {duration_str:?}: {e}")))?;

    let local_fut = tokio::task::spawn_blocking(move || -> Result<Vec<(Uuid, i64, String)>, ErrorObject<'static>> {
        let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
        db.collect_fingerprints_with_ts_in_recent(dur)
            .map_err(|e| rpc_err(-32004, e))
    });

    let cluster = bdslib::get_db().ok().and_then(|d| d.cluster().cloned());
    let params  = serde_json::json!({ "duration": duration_str });
    let fanout_fut = async {
        match &cluster {
            Some(c) => Some(fanout::fan_out_v2(c, "v2/fingerprints.recent_timed", params).await),
            None    => None,
        }
    };

    let (local_res, fan) = tokio::join!(local_fut, fanout_fut);
    let local_triples = local_res
        .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))??;

    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut events: Vec<(i64, String)> = Vec::new();
    let mut raw_total = local_triples.len();
    for (id, ts, fp) in local_triples {
        if seen.insert(id) { events.push((ts, fp)); }
    }
    if let Some(f) = &fan {
        for body in f.ok_results() {
            let arr = match body.get("fingerprints").and_then(|v| v.as_array()) {
                Some(a) => a,
                None    => continue,
            };
            raw_total += arr.len();
            for item in arr {
                let id_str = match item.get("id").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None    => continue,
                };
                let id = match Uuid::parse_str(id_str) {
                    Ok(u)  => u,
                    Err(_) => continue,
                };
                if !seen.insert(id) { continue; }
                let ts = item.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
                if let Some(fp) = item.get("fingerprint").and_then(|v| v.as_str()) {
                    if !fp.trim().is_empty() {
                        events.push((ts, fp.to_owned()));
                    }
                }
            }
        }
    }

    // Markov projection needs chronological order so the empirical-
    // gap learner observes positive inter-arrival times.  Fan-out
    // completion order is arbitrary; sort once here.
    events.sort_by_key(|(ts, _)| *ts);

    Ok(ClusterFingerprintsTimed { events, raw_total, fan })
}
