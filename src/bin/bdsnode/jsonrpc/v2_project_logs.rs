//! `v2/project_logs` — project the next likely events on this node only.
//!
//! Reads primary records from `[now − duration_back, now)`, trains a
//! semi-Markov chain (default order 2, drain3 bucketing) over them, and
//! returns the modal projected events for the next `duration_forward`
//! aggregated across `n_samples` Monte Carlo rollouts.
//!
//! See `v3/project_logs` for the cluster-wide variant that fans out and
//! runs the projection once over the union of every peer's recent
//! corpora.

use super::params::rpc_err;
use bdslib::analysis::markov::{
    Bucketing, MarkovProjectionConfig,
};
use jsonrpsee::types::ErrorObject;
use jsonrpsee::RpcModule;
use serde::Deserialize;
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectLogsParams {
    /// Humantime lookback for training data.  Default: `"1h"`.
    #[serde(default = "default_duration_back")]
    pub duration_back: String,
    /// Humantime projection window.  Default: `"30min"`.
    #[serde(default = "default_duration_forward")]
    pub duration_forward: String,

    // Optional tuning — every one of these falls through to
    // `MarkovProjectionConfig::default()` when absent.
    #[serde(default)] pub order:             Option<u8>,
    #[serde(default)] pub n_samples:         Option<usize>,
    #[serde(default)] pub time_bins:         Option<usize>,
    #[serde(default)] pub min_consensus:     Option<f64>,
    #[serde(default)] pub events_per_second: Option<f64>,
    #[serde(default)] pub max_events:        Option<usize>,
    #[serde(default)] pub smoothing:         Option<f64>,
    #[serde(default)] pub seed:              Option<u64>,
    /// `"drain3"` (default), `"normalize"`, or `"identity"`.
    #[serde(default)] pub bucketing:         Option<String>,
}

fn default_duration_back()    -> String { "1h".to_owned() }
fn default_duration_forward() -> String { "30min".to_owned() }

pub fn build_cfg(p: &ProjectLogsParams) -> MarkovProjectionConfig {
    let mut cfg = MarkovProjectionConfig::default();
    if let Some(v) = p.order             { cfg.order             = v; }
    if let Some(v) = p.n_samples         { cfg.n_samples         = v; }
    if let Some(v) = p.time_bins         { cfg.time_bins         = v; }
    if let Some(v) = p.min_consensus     { cfg.min_consensus     = v; }
    if let Some(v) = p.events_per_second { cfg.events_per_second = v; }
    if let Some(v) = p.max_events        { cfg.max_events        = v; }
    if let Some(v) = p.smoothing         { cfg.smoothing         = v; }
    cfg.seed = p.seed;
    if let Some(s) = &p.bucketing {
        cfg.bucketing = match s.to_ascii_lowercase().as_str() {
            "drain3"    => Bucketing::Drain3,
            "normalize" => Bucketing::Normalize,
            "identity"  => Bucketing::Identity,
            _           => cfg.bucketing,
        };
    }
    cfg
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/project_logs", |params, _ctx, _| async move {
            log::debug!("v2/project_logs: start");
            let p: ProjectLogsParams = params.parse()?;
            let cfg = build_cfg(&p);
            let duration_back    = p.duration_back.clone();
            let duration_forward = p.duration_forward.clone();
            let started = std::time::Instant::now();
            let result = tokio::task::spawn_blocking(move || {
                let dur_back = humantime::parse_duration(&duration_back)
                    .map_err(|e| rpc_err(-32600,
                        format!("invalid duration_back {duration_back:?}: {e}")))?;
                let db = bdslib::get_db().map_err(|e| rpc_err(-32001, e))?;
                let events = db.project_logs_recent(dur_back, &duration_forward, &cfg)
                    .map_err(|e| rpc_err(-32004, e))?;
                let arr: Vec<JsonValue> = events.iter().map(|e| serde_json::json!({
                    "offset_secs":     e.offset_secs,
                    "text":            e.text,
                    "source_state":    e.source_state,
                    "transition_prob": e.transition_prob,
                })).collect();
                Ok::<JsonValue, ErrorObject>(serde_json::json!({
                    "n":                arr.len(),
                    "duration_back":    duration_back,
                    "duration_forward": duration_forward,
                    "events":           arr,
                }))
            })
            .await
            .map_err(|e| rpc_err(-32000, format!("task panicked: {e}")))?;
            let elapsed_us = started.elapsed().as_micros() as u64;
            bdslib::perf::record_us("v2/project_logs", elapsed_us);
            log::debug!("v2/project_logs: done");
            result
        })
        .unwrap();
}
