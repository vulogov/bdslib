//! `v3/denoise.recent` — distributed n-gram noise removal.
//!
//! Same shape as `v2/denoise.recent`, but the input corpus is the union
//! of every peer's `v2/fingerprints.recent` (deduped by UUID).  The
//! analysis function (`ngram_remove_noise_with`) runs once on the
//! coordinator over the union — noise scores are corpus-relative, so
//! per-peer analysis would silently mis-classify.

use super::params::{rpc_err, v3_cluster_meta};
use super::v3_helpers::gather_cluster_fingerprints;
use bdslib::analysis::ngram::{ngram_remove_noise_with, NgramNoiseConfig};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde_json::Value as JsonValue;

fn default_n()              -> usize { 2 }
fn default_min_word_len()   -> usize { 2 }
fn default_noise_threshold()-> f32   { 0.85 }
fn default_max_kept()       -> usize { 100 }
fn default_max_removed()    -> usize { 100 }

#[derive(serde::Deserialize)]
struct Params {
    #[serde(default)] #[allow(dead_code)] session: String,
    duration: String,
    #[serde(default = "default_n")] n: usize,
    #[serde(default = "default_min_word_len")] min_word_len: usize,
    #[serde(default = "default_noise_threshold")] noise_threshold: f32,
    #[serde(default = "default_max_kept")] max_kept: usize,
    #[serde(default = "default_max_removed")] max_removed: usize,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v3/denoise.recent", |params, _ctx, _| async move {
            log::debug!("v3/denoise.recent: start");
            let p: Params = params.parse()?;

            let bundle = gather_cluster_fingerprints(&p.duration).await?;
            let cfg = NgramNoiseConfig {
                n:               p.n,
                min_word_len:    p.min_word_len,
                noise_threshold: p.noise_threshold,
                max_kept:        p.max_kept,
                max_removed:     p.max_removed,
            };

            let fingerprints = bundle.fingerprints;
            let raw_total    = bundle.raw_total;
            let n_unique     = fingerprints.len();
            let analysis = tokio::task::spawn_blocking(move || ngram_remove_noise_with(&fingerprints, &cfg))
                .await
                .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;

            let mut out = analysis;
            if let Some(obj) = out.as_object_mut() {
                obj.insert("cluster_meta".into(), v3_cluster_meta(bundle.fan));
                obj.insert("n_unique_fingerprints".into(), JsonValue::from(n_unique));
                obj.insert("n_raw_fingerprints".into(), JsonValue::from(raw_total));
            }

            log::debug!("v3/denoise.recent: done");
            Ok::<JsonValue, ErrorObject>(out)
        })
        .unwrap();
}
