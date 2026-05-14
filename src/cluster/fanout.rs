//! Fan-out helper for v3/* distributed reads.
//!
//! Calls a `v2/*` method on every Alive peer in parallel, returning per-peer
//! results plus a coarse partial-failure flag.  Coordinator-side handlers
//! (in `bdsnode/jsonrpc/v3_*.rs`) compose this with their own local result
//! to produce a deduplicated answer.
//!
//! Outbound calls do **not** carry HMAC: v3/* distributed reads are issued
//! by trusted local clients (the same trust boundary as v2/*), and the
//! receiver-side v2/* handlers are unauthenticated.

use crate::cluster::peer_table::Peer;
use crate::cluster::Cluster;
use crate::common::error::{err_msg, Result};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;

/// Outcome of a single peer call.
pub struct PeerResult {
    pub peer:   Peer,
    pub result: Result<JsonValue>,
}

impl std::fmt::Debug for PeerResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerResult")
            .field("node_id", &self.peer.node_id)
            .field("url",     &self.peer.url)
            .field("ok",      &self.result.is_ok())
            .finish()
    }
}

/// Aggregate fan-out result.  `cluster_meta()` produces the JSON block
/// that v3/* responses embed so callers can detect partial answers.
pub struct FanOutResults {
    pub responses:      Vec<PeerResult>,
    pub peers_queried:  usize,
    pub peers_answered: usize,
}

impl FanOutResults {
    pub fn partial(&self) -> bool {
        self.peers_answered < self.peers_queried
    }

    /// Iterate over successful peer results only.
    pub fn ok_results(&self) -> impl Iterator<Item = &JsonValue> {
        self.responses.iter().filter_map(|r| r.result.as_ref().ok())
    }

    /// Standard `cluster_meta` block embedded in every v3/* read response.
    /// Callers add `"local"` themselves after calling the local handler.
    pub fn cluster_meta(&self) -> JsonValue {
        let failed: Vec<JsonValue> = self.responses.iter()
            .filter_map(|r| r.result.as_ref().err().map(|e| serde_json::json!({
                "node_id": r.peer.node_id.to_string(),
                "url":     r.peer.url,
                "error":   e.to_string(),
            })))
            .collect();
        serde_json::json!({
            "peers_queried":  self.peers_queried,
            "peers_answered": self.peers_answered,
            "partial":        self.partial(),
            "failed":         failed,
        })
    }
}

/// Issue `method` against every Alive peer with the supplied `params`.
/// Returns a `FanOutResults` with one entry per peer (success or error).
///
/// Per-peer timeout = `cluster.peer_rpc_timeout`.  The local node is **not**
/// queried — coordinator handlers do that in-process before/after this.
pub async fn fan_out_v2(
    cluster: &Arc<Cluster>,
    method:  &str,
    params:  JsonValue,
) -> FanOutResults {
    let alive = cluster.peers.read().alive();
    let default_timeout = Duration::from_secs(cluster.config.peer_rpc_timeout_secs);

    if alive.is_empty() {
        return FanOutResults { responses: vec![], peers_queried: 0, peers_answered: 0 };
    }

    let adaptive = cluster.config.adaptive_peer_timeout_enabled;
    let multiplier = cluster.config.adaptive_peer_timeout_multiplier;

    let mut set: JoinSet<PeerResult> = JoinSet::new();
    for peer in alive.iter().cloned() {
        let http   = cluster.http.clone();
        let url    = peer.url.clone();
        let method = method.to_owned();
        let params = params.clone();
        let node_label = peer.node_id.to_string();

        // Per-peer dynamic deadline.  Clamps to `[default × 0.1, default]`.
        // Falls back to the static default whenever the peer has fewer
        // than 20 recent samples (insufficient signal).
        let timeout = if adaptive {
            adaptive_timeout(&node_label, default_timeout, multiplier)
        } else {
            default_timeout
        };

        set.spawn(async move {
            // Time the RPC for the cluster perf registry.  Two labels:
            //   fanout.peer.<node_id>     — per-peer RTT distribution
            //   fanout.method.<method>    — per-RPC RTT across peers
            // Both populate p50/p95/p99 for v2/perf.
            let started = std::time::Instant::now();
            let result = call_v2(&http, &url, &method, params, timeout).await;
            let elapsed_us = started.elapsed().as_micros() as u64;
            crate::perf::record_us(&format!("fanout.peer.{node_label}"), elapsed_us);
            crate::perf::record_us(&format!("fanout.method.{method}"),  elapsed_us);
            PeerResult { peer, result }
        });
    }

    let mut responses = Vec::with_capacity(alive.len());
    let mut answered  = 0;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(r) => {
                if r.result.is_ok() { answered += 1; }
                responses.push(r);
            }
            Err(e) => log::warn!("[cluster] fan_out_v2 task panicked: {e:?}"),
        }
    }
    FanOutResults {
        peers_queried: alive.len(),
        peers_answered: answered,
        responses,
    }
}

/// Compute the per-peer RPC deadline.
///
/// Returns the static `default` unchanged when the peer has not
/// accumulated enough samples (less than 20 in the ring) for its p95
/// to be trustworthy.  Otherwise returns
/// `min(default, p95 × multiplier).max(default / 10)` —
/// never exceeds the operator's contract, never drops below 10% of it.
///
/// 10% floor protects against degenerate cases where a series's p95 is
/// momentarily 0 (cold path) or near-zero from a single fast sample.
fn adaptive_timeout(node_label: &str, default: Duration, multiplier: f64) -> Duration {
    let series = format!("fanout.peer.{node_label}");
    let Some(p95_us) = crate::perf::registry().p95_us(&series, 20) else {
        return default;
    };
    let scaled_us = (p95_us as f64) * multiplier;
    if !scaled_us.is_finite() || scaled_us <= 0.0 {
        return default;
    }
    let default_us = default.as_micros() as f64;
    let floor_us   = (default_us * 0.1).max(1_000.0); // never below 1 ms
    let clamped    = scaled_us.clamp(floor_us, default_us);
    Duration::from_micros(clamped as u64)
}

async fn call_v2(
    http:    &reqwest::Client,
    url:     &str,
    method:  &str,
    params:  JsonValue,
    timeout: Duration,
) -> Result<JsonValue> {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method":  method,
        "params":  params,
        "id":      1,
    });
    let body = serde_json::to_string(&payload)
        .map_err(|e| err_msg(format!("serialize fanout body: {e}")))?;

    let resp = http.post(url)
        .timeout(timeout)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| err_msg(format!("{method} -> {url}: {e}")))?;

    let text = resp.text().await
        .map_err(|e| err_msg(format!("{method} <- {url}: read body: {e}")))?;
    let envelope: JsonValue = serde_json::from_str(&text)
        .map_err(|e| err_msg(format!("{method} <- {url}: invalid JSON: {e}")))?;

    if let Some(err) = envelope.get("error") {
        let msg = err.get("message").and_then(|v| v.as_str()).unwrap_or("unknown");
        return Err(err_msg(format!("{method} rpc error: {msg}")));
    }
    Ok(envelope.get("result").cloned().unwrap_or_default())
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests — adaptive_timeout is pure; no Cluster needed.
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Push n samples of `value_us` into a series so its p95 reads exactly.
    fn seed_series(name: &str, value_us: u64, n: usize) {
        for _ in 0..n {
            crate::perf::record(name, value_us);
        }
    }

    #[test]
    fn adaptive_returns_default_when_too_few_samples() {
        let name = "test.adapt.few";
        seed_series(name, 10_000, 5); // 5 samples — below 20 min
        let got = adaptive_timeout("adapt.few", Duration::from_secs(2), 3.0);
        assert_eq!(got, Duration::from_secs(2));
    }

    #[test]
    fn adaptive_tightens_to_p95_times_multiplier() {
        let name = "fanout.peer.adapt.healthy";
        // 50 samples at 100 ms → p95 ≈ 100 ms.  Multiplier 3 → 300 ms.
        seed_series(name, 100_000, 50);
        let got = adaptive_timeout("adapt.healthy", Duration::from_secs(2), 3.0);
        let got_ms = got.as_millis();
        assert!(got_ms >= 250 && got_ms <= 350, "got {got_ms} ms");
    }

    #[test]
    fn adaptive_never_exceeds_default() {
        let name = "fanout.peer.adapt.slow";
        // 50 samples at 5 s → multiplier × p95 = 15 s, but default is 2 s.
        seed_series(name, 5_000_000, 50);
        let got = adaptive_timeout("adapt.slow", Duration::from_secs(2), 3.0);
        assert_eq!(got, Duration::from_secs(2));
    }

    #[test]
    fn adaptive_never_drops_below_ten_percent_of_default() {
        let name = "fanout.peer.adapt.tiny";
        // 50 samples at 1 µs → multiplier × p95 = 3 µs ≪ 10% floor.
        seed_series(name, 1, 50);
        let got = adaptive_timeout("adapt.tiny", Duration::from_secs(2), 3.0);
        // Floor is 200 ms (10% of 2 s).
        let got_ms = got.as_millis();
        assert!(got_ms >= 199 && got_ms <= 201, "got {got_ms} ms");
    }
}
