//! `v3/knn` — distributed k-NN intelligence.
//!
//! Same parameters and output shape as `v2/knn`, but the input fingerprint
//! corpus is the union of every peer's `v2/fingerprints.recent` (deduped
//! by UUID).  The analysis function (`knn_summary_with`) runs **once** on
//! the coordinator over the union — running it per-peer and merging
//! summaries would be incorrect (cluster IDs and density rankings are
//! corpus-relative).

use super::params::{rpc_err, v3_cluster_meta};
use super::v3_helpers::gather_cluster_fingerprints;
use bdslib::analysis::knn::{knn_summary_with, KnnConfig};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

fn default_k()                   -> usize { 5 }
fn default_min_word_len()        -> usize { 2 }
fn default_anomaly_threshold()   -> f32   { 0.2 }
fn default_max_cluster_members() -> usize { 10 }
fn default_max_anomalies()       -> usize { 20 }

#[derive(serde::Deserialize)]
struct Params {
    #[serde(default)] #[allow(dead_code)] session: String,
    duration: String,
    #[serde(default = "default_k")] k: usize,
    #[serde(default = "default_min_word_len")] min_word_len: usize,
    #[serde(default = "default_anomaly_threshold")] anomaly_threshold: f32,
    #[serde(default = "default_max_cluster_members")] max_cluster_members: usize,
    #[serde(default = "default_max_anomalies")] max_anomalies: usize,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v3/knn", |params, _ctx, _| async move {
            log::debug!("v3/knn: start");
            let p: Params = params.parse()?;

            let bundle = gather_cluster_fingerprints(&p.duration).await?;
            let cfg = KnnConfig {
                k:                   p.k,
                min_word_len:        p.min_word_len,
                anomaly_threshold:   p.anomaly_threshold,
                max_cluster_members: p.max_cluster_members,
                max_anomalies:       p.max_anomalies,
            };

            // Run analysis on the dedup'd union, on a blocking thread.
            let fingerprints = bundle.fingerprints;
            let raw_total    = bundle.raw_total;
            let n_unique     = fingerprints.len();
            let analysis = tokio::task::spawn_blocking(move || knn_summary_with(&fingerprints, &cfg))
                .await
                .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;

            let mut out = analysis;
            if let Some(obj) = out.as_object_mut() {
                obj.insert("cluster_meta".into(), v3_cluster_meta(bundle.fan));
                obj.insert("n_unique_fingerprints".into(), JsonValue::from(n_unique));
                obj.insert("n_raw_fingerprints".into(), JsonValue::from(raw_total));
            }

            log::debug!("v3/knn: done");
            Ok::<JsonValue, ErrorObject>(out)
        })
        .unwrap();
}
