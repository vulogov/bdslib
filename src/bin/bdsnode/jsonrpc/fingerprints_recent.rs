//! `v2/fingerprints.recent` — return raw `(uuid, fingerprint)` pairs for
//! every primary record observed in the lookback window.
//!
//! Designed for the v3 distributed-analytics fan-out path: each peer
//! returns its local fingerprints, the coordinator dedups by UUID, and the
//! analysis (`knn_summary_with`, `ngram_anomaly_with`,
//! `ngram_remove_noise_with`) runs once over the union.  This is the only
//! mathematically-correct way to fan analysis across peers — running the
//! analysis per-peer and merging summaries gives wrong results because
//! KNN clusters / anomaly scores / noise scores are all corpus-relative.

use super::params::rpc_err;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;

#[derive(serde::Deserialize)]
struct Params {
    /// Lookback window in humantime notation (e.g. `"1h"`, `"30min"`).
    duration: String,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/fingerprints.recent", |params, _ctx, _| async move {
            log::debug!("v2/fingerprints.recent: start");
            let p: Params = params.parse()?;

            let result = tokio::task::spawn_blocking(move || {
                let dur = humantime::parse_duration(&p.duration)
                    .map_err(|e| rpc_err(-32600, format!("invalid duration {:?}: {e}", p.duration)))?;

                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let pairs = db
                    .fingerprints_with_ids_in_recent(dur)
                    .map_err(|e| rpc_err(-32004, e))?;

                let n = pairs.len();
                let arr: Vec<serde_json::Value> = pairs.into_iter()
                    .map(|(id, fp)| serde_json::json!({
                        "id":          id.to_string(),
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

            log::debug!("v2/fingerprints.recent: done");
            result
        })
        .unwrap();
}
