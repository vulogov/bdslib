//! `v3/project_logs` — cluster-wide semi-Markov projection.
//!
//! Same shape as `v2/project_logs`, but the input corpus is the union
//! of every peer's `v2/fingerprints.recent_timed`, deduped by UUID and
//! sorted chronologically.  The projection runs **once** on the
//! coordinator over the union — same "single-analysis-on-union"
//! correctness invariant `v3/anomaly.recent` / `v3/knn` / `v3/denoise`
//! already follow, because the empirical inter-arrival distribution
//! is corpus-relative and would silently mis-estimate if computed
//! per-peer.

use super::params::{rpc_err, v3_cluster_meta};
use super::v2_project_logs::{build_cfg, ProjectLogsParams};
use super::v3_helpers::gather_cluster_fingerprints_timed;
use bdslib::analysis::markov::markov_project_timed_with;
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v3/project_logs", |params, _ctx, _| async move {
            log::debug!("v3/project_logs: start");
            let p: ProjectLogsParams = params.parse()?;
            let cfg = build_cfg(&p);

            let bundle = gather_cluster_fingerprints_timed(&p.duration_back).await?;
            let events_in    = bundle.events;
            let raw_total    = bundle.raw_total;
            let n_unique     = events_in.len();
            let project_fwd  = p.duration_forward.clone();
            let started      = std::time::Instant::now();

            // Run the projection on a blocking thread — drain3
            // template mining (default bucketing) plus N rollouts of
            // O(events) sampling is CPU-bound; we don't want to park
            // on the async runtime.
            let projection = tokio::task::spawn_blocking(move || {
                markov_project_timed_with(&events_in, &project_fwd, &cfg)
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            let elapsed_us = started.elapsed().as_micros() as u64;
            bdslib::perf::record_us("v3/project_logs", elapsed_us);

            let arr: Vec<JsonValue> = projection.iter().map(|e| serde_json::json!({
                "offset_secs":     e.offset_secs,
                "text":            e.text,
                "source_state":    e.source_state,
                "transition_prob": e.transition_prob,
            })).collect();

            log::debug!("v3/project_logs: done");
            Ok::<JsonValue, ErrorObject>(serde_json::json!({
                "n":                arr.len(),
                "n_unique_inputs":  n_unique,
                "n_raw_inputs":     raw_total,
                "duration_back":    p.duration_back,
                "duration_forward": p.duration_forward,
                "events":           arr,
                "cluster_meta":     v3_cluster_meta(bundle.fan),
            }))
        })
        .unwrap();
}
