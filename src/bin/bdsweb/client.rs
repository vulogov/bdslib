use serde_json::{json, Value};
use std::time::{Duration, Instant};
use crate::{error::AppError, state::AppState};

pub const SESSION: &str = "bdsweb-ui-session";

/// TTL for the cached `cluster_enabled` flag.  Short enough to pick up
/// config changes within a couple page loads; long enough to avoid one
/// `v2/status` round-trip per Telemetry/Analysis/RCA page load.
const CLUSTER_MODE_TTL: Duration = Duration::from_secs(30);

/// Returns `true` when the bdsnode this bdsweb instance is talking to has
/// `cluster.enabled = true` in its bds.hjson.  Caches the answer for
/// `CLUSTER_MODE_TTL` so each page load doesn't pay an extra round-trip.
///
/// On error (bdsnode unreachable, malformed v2/status response) returns
/// `false` — i.e., assumes standalone and uses v2/* methods.  The cluster
/// page surfaces real connectivity errors separately.
pub async fn cluster_enabled(state: &AppState) -> bool {
    {
        let r = state.cluster_mode.read().await;
        if r.fetched_at.elapsed() < CLUSTER_MODE_TTL {
            return r.enabled;
        }
    }
    // Refresh.  v2/status returns `cluster: { … }` when on, `cluster: null`
    // (or absent) when off.
    let enabled = match rpc(state, "v2/status", json!({})).await {
        Ok(v) => v.get("cluster").map(|c| !c.is_null()).unwrap_or(false),
        Err(_) => false,
    };
    *state.cluster_mode.write().await = crate::state::ClusterModeCache {
        enabled,
        fetched_at: Instant::now(),
    };
    enabled
}

/// Call `v3_method` when cluster mode is on, otherwise `v2_method`.  Use
/// for read RPCs that have a cluster-aware v3/* counterpart.  Both
/// methods must accept the same params shape — hold for every v3/* read
/// added in Phase 6 + the v3/{anomaly.recent,denoise.recent,knn,rca,
/// rca.templates} family.
pub async fn rpc_versioned(
    state:     &AppState,
    v2_method: &str,
    v3_method: &str,
    params:    Value,
) -> Result<Value, AppError> {
    let method = if cluster_enabled(state).await { v3_method } else { v2_method };
    rpc(state, method, params).await
}

pub async fn rpc(state: &AppState, method: &str, params: Value) -> Result<Value, AppError> {
    let payload = json!({
        "jsonrpc": "2.0",
        "method":  method,
        "params":  params,
        "id":      1
    });

    let body = serde_json::to_string(&payload)?;
    let resp  = state.http
        .post(state.node_url.as_str())
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await?;

    let text: String = resp.text().await?;
    let envelope: Value = serde_json::from_str(&text)?;

    if let Some(err) = envelope.get("error") {
        let msg = err["message"].as_str().unwrap_or("unknown RPC error").to_owned();
        return Err(AppError::Rpc(msg));
    }

    Ok(envelope["result"].clone())
}

// ── Small helpers to pull typed scalars out of JSON safely ────────────────────

pub fn str_val(v: &Value, key: &str) -> String {
    v.get(key)
     .and_then(|x| x.as_str())
     .unwrap_or("—")
     .to_owned()
}

pub fn u64_val(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

pub fn fmt_ts(unix_secs: u64) -> String {
    use chrono::{TimeZone, Utc};
    if unix_secs == 0 { return "—".to_owned(); }
    Utc.timestamp_opt(unix_secs as i64, 0)
       .single()
       .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
       .unwrap_or_else(|| "—".to_owned())
}
