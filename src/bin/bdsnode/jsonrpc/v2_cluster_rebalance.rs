//! `v2/cluster.has_records` + `v2/cluster.replicate_record` — the
//! receiver side of the background data rebalancer.
//!
//! Both are unauthenticated v2/* calls — same trust boundary as
//! `v2/cluster.peers` and the other rebalance-adjacent RPCs.  The
//! cluster's shared secret gates v3/* admin surface, not the
//! receiver-side replication helpers, which need to be cheap and
//! reachable by any peer in the gossip ring.
//!
//! Both reuse the existing [`bdslib::ShardsManager`] helpers
//! ([`has_records_present`] and [`replicate_record`]) — the RPC
//! handlers are thin marshalling wrappers.

use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct HasRecordsParams {
    /// UUIDs to probe (as strings).  Invalid strings are silently
    /// skipped — they can't match anything anyway.
    ids: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ReplicateRecordParams {
    /// Full record to replicate.  Must contain `id`, `timestamp`,
    /// `key`, and `data` — same shape `v2/add` expects.
    record: JsonValue,
}

pub fn register(module: &mut RpcModule<()>) {
    // ── v2/cluster.has_records ────────────────────────────────────────
    module
        .register_async_method("v2/cluster.has_records", |params, _ctx, _| async move {
            let p: HasRecordsParams = params.parse()?;
            let started = std::time::Instant::now();
            let result = tokio::task::spawn_blocking(move || {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let parsed: Vec<Uuid> = p.ids.iter()
                    .filter_map(|s| Uuid::parse_str(s).ok())
                    .collect();
                let present = db.has_records_present(&parsed)
                    .map_err(|e| rpc_err(-32004, e))?;
                let present_strings: Vec<String> =
                    present.iter().map(|u| u.to_string()).collect();
                Ok::<JsonValue, ErrorObject>(serde_json::json!({
                    "n_probed":  parsed.len(),
                    "n_present": present_strings.len(),
                    "present":   present_strings,
                }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            bdslib::perf::record_us("rebalancer.has_records",
                started.elapsed().as_micros() as u64);
            result
        })
        .unwrap();

    // ── v2/cluster.replicate_record ───────────────────────────────────
    module
        .register_async_method("v2/cluster.replicate_record", |params, _ctx, _| async move {
            let p: ReplicateRecordParams = params.parse()?;
            let started = std::time::Instant::now();
            let result = tokio::task::spawn_blocking(move || {
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let (stored_id, was_new) = db.replicate_record(p.record)
                    .map_err(|e| rpc_err(-32004, e))?;
                Ok::<JsonValue, ErrorObject>(serde_json::json!({
                    "id":      stored_id.to_string(),
                    "was_new": was_new,
                }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            bdslib::perf::record_us("rebalancer.replicate_record_recv",
                started.elapsed().as_micros() as u64);
            result
        })
        .unwrap();
}
