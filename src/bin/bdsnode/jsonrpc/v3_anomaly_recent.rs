//! `v3/anomaly.recent` — distributed n-gram phrase-rarity outliers.
//!
//! Same shape as `v2/anomaly.recent`, but the input corpus is the union
//! of every peer's `v2/fingerprints.recent` (deduped by UUID).  The
//! analysis function (`ngram_anomaly_with`) runs once on the coordinator
//! over the union — anomaly scores are corpus-relative, so per-peer
//! analysis would silently mis-rank.

use super::params::{rpc_err, v3_cluster_meta};
use super::v3_helpers::gather_cluster_fingerprints;
use bdslib::analysis::ngram::{ngram_anomaly_with, NgramAnomalyConfig};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

fn default_n()                 -> usize { 2 }
fn default_min_word_len()      -> usize { 2 }
fn default_anomaly_threshold() -> f32   { 0.7 }
fn default_max_anomalies()     -> usize { 20 }
fn default_max_novel_ngrams()  -> usize { 5 }

#[derive(serde::Deserialize)]
struct Params {
    #[serde(default)] #[allow(dead_code)] session: String,
    duration: String,
    #[serde(default = "default_n")] n: usize,
    #[serde(default = "default_min_word_len")] min_word_len: usize,
    #[serde(default = "default_anomaly_threshold")] anomaly_threshold: f32,
    #[serde(default = "default_max_anomalies")] max_anomalies: usize,
    #[serde(default = "default_max_novel_ngrams")] max_novel_ngrams: usize,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v3/anomaly.recent", |params, _ctx, _| async move {
            log::debug!("v3/anomaly.recent: start");
            let p: Params = params.parse()?;

            let bundle = gather_cluster_fingerprints(&p.duration).await?;
            let cfg = NgramAnomalyConfig {
                n:                 p.n,
                min_word_len:      p.min_word_len,
                anomaly_threshold: p.anomaly_threshold,
                max_anomalies:     p.max_anomalies,
                max_novel_ngrams:  p.max_novel_ngrams,
            };

            let fingerprints = bundle.fingerprints;
            let raw_total    = bundle.raw_total;
            let n_unique     = fingerprints.len();
            let analysis = tokio::task::spawn_blocking(move || ngram_anomaly_with(&fingerprints, &cfg))
                .await
                .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;

            let mut out = analysis;
            if let Some(obj) = out.as_object_mut() {
                obj.insert("cluster_meta".into(), v3_cluster_meta(bundle.fan));
                obj.insert("n_unique_fingerprints".into(), JsonValue::from(n_unique));
                obj.insert("n_raw_fingerprints".into(), JsonValue::from(raw_total));
            }

            log::debug!("v3/anomaly.recent: done");
            Ok::<JsonValue, ErrorObject>(out)
        })
        .unwrap();
}
