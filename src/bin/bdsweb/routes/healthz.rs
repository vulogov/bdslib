//! `GET /healthz` — readiness probe reflecting bdsnode connectivity.
//!
//! bdsweb is "up" as long as it can serve cached pages and `/login`,
//! but an operator or load balancer also wants to know whether its
//! *backend* is reachable.  The dashboard background poller already
//! answers that question every interval; `/healthz` just exposes its
//! last-success timestamp as an `ok` / `starting` / `degraded`
//! verdict.  No extra RPC — it reads process-local atomic state only.
//!
//! Sits in the auth-middleware allow-list (like `/version`) so probes
//! work without a session.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::state::AppState;

/// Verdict semantics:
/// - `starting` (200) — the poller hasn't completed a first fetch yet.
/// - `ok`       (200) — last success is within ~3 poll intervals.
/// - `degraded` (503) — last success is older than that; bdsnode looks
///   unreachable, so a strict LB can drain this instance.
pub async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last_ok  = state.dashboard_last_ok.load(Ordering::Relaxed);
    let interval = state.dashboard_refresh_secs.max(1);
    // Allow ~3 missed poll cycles before calling it degraded.
    let stale_after = interval.saturating_mul(3);

    let (status, code) = if last_ok == 0 {
        ("starting", StatusCode::OK)
    } else if now.saturating_sub(last_ok) <= stale_after {
        ("ok", StatusCode::OK)
    } else {
        ("degraded", StatusCode::SERVICE_UNAVAILABLE)
    };

    let age: Value = if last_ok == 0 {
        Value::Null
    } else {
        json!(now.saturating_sub(last_ok))
    };

    let body = json!({
        "status": status,
        "bdsnode": {
            "last_ok_unix":       last_ok,
            "age_secs":           age,
            "poll_interval_secs": interval,
        },
        "ts": now,
    });
    (code, Json(body))
}
