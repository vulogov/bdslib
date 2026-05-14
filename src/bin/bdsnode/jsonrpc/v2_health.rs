//! `v2/health` — dedicated readiness / liveness probe.
//!
//! Returns the aggregate verdict from [`bdslib::health`] plus the
//! per-source breakdown.  Designed for load balancers and
//! orchestrators (k8s readiness/liveness, HAProxy health checks) —
//! cheap, in-process, no DB access, and a stable JSON shape:
//!
//! ```json
//! {
//!   "status":  "healthy" | "degraded" | "failed",
//!   "reason":  "...",          // empty when healthy
//!   "ts":      1778900000,
//!   "sources": [
//!     { "name": "ingest.flushers", "status": "healthy",
//!       "reason": "", "last_heartbeat": 1778899998, "stale": false }
//!   ]
//! }
//! ```
//!
//! Unlike `v2/status` (which is a broad operational snapshot), this
//! method answers exactly one question — *should traffic come here?* —
//! and answers it from the in-process health registry alone.

use jsonrpsee::RpcModule;

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/health", |_params, _ctx, _| async move {
            let reg = bdslib::health::registry();
            let verdict = reg.verdict();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let sources: Vec<serde_json::Value> = reg.snapshot()
                .into_iter()
                .map(|r| {
                    let eff = r.effective(now);
                    serde_json::json!({
                        "name":           r.name,
                        "status":         eff.label(),
                        "reason":         eff.reason(),
                        "last_heartbeat": r.last_heartbeat,
                        "stale":          r.is_stale(now),
                    })
                })
                .collect();

            Ok::<serde_json::Value, jsonrpsee::types::ErrorObject>(serde_json::json!({
                "status":  verdict.label(),
                "reason":  verdict.reason(),
                "ts":      now,
                "sources": sources,
            }))
        })
        .unwrap();
}
