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

/// Visual badge shown on Telemetry / Analysis / RCA result pages telling
/// the operator whether the request went through the cluster (v3/*) or
/// the local node only (v2/*).  Each affected route builds this from
/// the v3 response's `cluster_meta` block, or from the cached
/// `cluster_enabled` flag when the response didn't carry one.
#[derive(Debug, Clone)]
pub struct ModeBadge {
    /// Display text: `"Cluster"` or `"Standalone"`.
    pub label:   String,
    /// Tailwind colour class for the badge text.
    pub class:   String,
    /// Suffix like ` · 2/2 peers` (empty when not in cluster mode).
    pub extra:   String,
    /// `title=` tooltip with a one-line explanation.
    pub tooltip: String,
}

impl ModeBadge {
    /// Build from a v3 response containing a `cluster_meta` object.
    /// Falls back to `from_enabled(false)` when the response has no
    /// cluster_meta (i.e. we called v2/* — standalone mode).
    pub fn from_response(resp: &Value) -> Self {
        let meta = match resp.get("cluster_meta") {
            Some(m) if m.is_object() => m,
            _ => return Self::from_enabled(false),
        };
        let enabled = meta.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        if !enabled {
            return Self::from_enabled(false);
        }
        let queried  = meta.get("peers_queried").and_then(|v| v.as_u64()).unwrap_or(0);
        let answered = meta.get("peers_answered").and_then(|v| v.as_u64()).unwrap_or(0);
        let partial  = meta.get("partial").and_then(|v| v.as_bool()).unwrap_or(false);

        let class = if partial { "text-yellow-400" } else { "text-emerald-400" };
        let extra = format!(" · {answered}/{queried} peer{}", if queried == 1 { "" } else { "s" });
        let tooltip = format!(
            "Cluster mode — {answered}/{queried} peer{} answered.{}",
            if queried == 1 { "" } else { "s" },
            if partial { "  Partial response: some peers did not reply within the timeout." } else { "" },
        );
        Self { label: "Cluster".to_owned(), class: class.to_owned(), extra, tooltip }
    }

    /// Build a badge for cases where the response carries no
    /// cluster_meta (RPC failed, or v2/* was called without going
    /// through `rpc_versioned`).  Uses just the on/off flag.
    pub fn from_enabled(enabled: bool) -> Self {
        if enabled {
            Self {
                label:   "Cluster".to_owned(),
                class:   "text-emerald-400".to_owned(),
                extra:   String::new(),
                tooltip: "Cluster mode (no per-call peer info available).".to_owned(),
            }
        } else {
            Self {
                label:   "Standalone".to_owned(),
                class:   "text-slate-400".to_owned(),
                extra:   String::new(),
                tooltip: "Standalone mode — query ran against this node only.".to_owned(),
            }
        }
    }

    /// Badge for v2-only methods running on a clustered bdsnode.  The
    /// node IS in cluster mode but the operation itself has no v3
    /// counterpart, so it only sees this node's local data — operators
    /// should know they're looking at a partial view of the cluster.
    pub fn local_only_in_cluster() -> Self {
        Self {
            label:   "Local node".to_owned(),
            class:   "text-yellow-400".to_owned(),
            extra:   " (no cluster variant)".to_owned(),
            tooltip: "Cluster mode is enabled on this node, but this operation has no cluster-aware \
                      v3/* counterpart — the result reflects only this node's local data.".to_owned(),
        }
    }
}

/// Pick the right badge to render on a **page header** given whether the
/// underlying RPC has a v3/* variant.  Reads the cached cluster_enabled
/// flag so each page load doesn't pay an extra round-trip.
///
/// - `has_v3 == true,  cluster on`  → "via Cluster"      (emerald)
/// - `has_v3 == true,  cluster off` → "via Standalone"   (slate)
/// - `has_v3 == false, cluster on`  → "via Local node"   (yellow)
/// - `has_v3 == false, cluster off` → "via Standalone"   (slate)
pub async fn mode_badge_for_page(state: &AppState, has_v3: bool) -> ModeBadge {
    let enabled = cluster_enabled(state).await;
    if enabled && !has_v3 {
        ModeBadge::local_only_in_cluster()
    } else {
        ModeBadge::from_enabled(enabled)
    }
}

pub async fn rpc(state: &AppState, method: &str, params: Value) -> Result<Value, AppError> {
    rpc_with_timeout(state, method, params, None).await
}

/// `rpc` variant that lets the caller override the per-request
/// timeout.  Pass `None` to use the global client timeout (set in
/// `AppState::new`, currently 120 s).  Pass `Some(d)` for endpoints
/// that legitimately run longer — e.g. `v4/llm.analyze` with a fat
/// prompt against a local CPU-bound Ollama, which routinely needs
/// 60–300 s for 50+ rows.
pub async fn rpc_with_timeout(
    state:   &AppState,
    method:  &str,
    params:  Value,
    timeout: Option<std::time::Duration>,
) -> Result<Value, AppError> {
    let payload = json!({
        "jsonrpc": "2.0",
        "method":  method,
        "params":  params,
        "id":      1
    });

    let body = serde_json::to_string(&payload)?;
    let mut req = state.http
        .post(state.node_url.as_str())
        .header("Content-Type", "application/json")
        .body(body);
    if let Some(d) = timeout {
        req = req.timeout(d);
    }
    let resp = req.send().await?;

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
