//! `v2/fingerprints.recent_timed` — return `(uuid, ts, fingerprint)`
//! triples for every primary record in the lookback window.
//!
//! The timed sibling of `v2/fingerprints.recent`: same shape, plus the
//! integer `ts` (Unix seconds) column.  Designed as the per-node
//! primitive that `v3/project_logs` fans out to: the coordinator
//! collects triples from every Alive peer, dedupes by UUID, sorts
//! chronologically, and feeds the union into
//! `crate::analysis::markov::markov_project_timed_with` — same
//! mathematically-correct "single analysis on the union" pattern that
//! `v3/knn`, `v3/anomaly.recent`, and `v3/denoise.recent` already use.

use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;

#[derive(serde::Deserialize)]
struct Params {
    duration: String,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/fingerprints.recent_timed", |params, _ctx, _| async move {
            log::debug!("v2/fingerprints.recent_timed: start");
            let p: Params = params.parse()?;

            let result = tokio::task::spawn_blocking(move || {
                let dur = humantime::parse_duration(&p.duration)
                    .map_err(|e| rpc_err(-32600, format!("invalid duration {:?}: {e}", p.duration)))?;

                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let triples = db
                    .collect_fingerprints_with_ts_in_recent(dur)
                    .map_err(|e| rpc_err(-32004, e))?;

                let n = triples.len();
                let arr: Vec<serde_json::Value> = triples.into_iter()
                    .map(|(id, ts, fp)| serde_json::json!({
                        "id":          id.to_string(),
                        "ts":          ts,
                        "fingerprint": fp,
                    }))
                    .collect();

                Ok::<serde_json::Value, ErrorObject>(serde_json::json!({
                    "n":            n,
                    "fingerprints": arr,
                }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;

            log::debug!("v2/fingerprints.recent_timed: done");
            result
        })
        .unwrap();
}
