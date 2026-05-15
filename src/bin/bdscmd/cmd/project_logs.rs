//! `bdscmd project-logs` — cluster-wide semi-Markov log/event projection.
//!
//! Drives `v3/project_logs`: every Alive peer's recent primary records
//! get unioned (deduped by UUID, sorted chronologically), a
//! drain3-bucketed semi-Markov chain is trained on the union, and the
//! result is `n_samples` Monte Carlo rollouts aggregated into the modal
//! projected event per time bin.
//!
//! See `bdslib::analysis::markov::MarkovProjectionConfig` for the
//! per-knob semantics — every flag here maps one-to-one to a field.

use anyhow::Result;
use clap::Args;
use serde_json::{Map, Value};

#[derive(Args)]
pub struct Cmd {
    /// Humantime lookback for training data (e.g. "1h", "30min", "24h").
    #[arg(long, default_value = "1h")]
    duration_back: String,

    /// Humantime projection window — how far to look forward.
    #[arg(long, default_value = "30min")]
    duration_forward: String,

    /// Markov order K (1..=4; default 2).
    #[arg(long)]
    order: Option<u8>,

    /// Number of Monte Carlo rollouts to aggregate.
    #[arg(long)]
    n_samples: Option<usize>,

    /// Number of time bins to slice `duration_forward` into.
    #[arg(long)]
    time_bins: Option<usize>,

    /// Minimum share-of-samples consensus before an event is emitted.
    /// `0.0` keeps every observed projection; `0.5` requires majority.
    #[arg(long)]
    min_consensus: Option<f64>,

    /// Cap on the projected events returned.
    #[arg(long)]
    max_events: Option<usize>,

    /// Additive Laplace-style smoothing within a matched context.
    #[arg(long)]
    smoothing: Option<f64>,

    /// State bucketing — "drain3" (default), "normalize", or "identity".
    #[arg(long)]
    bucketing: Option<String>,

    /// RNG seed; set for reproducible projections.
    #[arg(long)]
    seed: Option<u64>,

    /// Fallback rate when the timed-gap learner has no observation for
    /// a transition.  Default 1.0 events/sec.
    #[arg(long)]
    events_per_second: Option<f64>,
}

pub fn run(url: &str, _session: &str, args: Cmd) -> Result<Value> {
    let mut params = Map::new();
    params.insert("duration_back".into(),    Value::String(args.duration_back));
    params.insert("duration_forward".into(), Value::String(args.duration_forward));
    if let Some(v) = args.order             { params.insert("order".into(),             Value::from(v)); }
    if let Some(v) = args.n_samples         { params.insert("n_samples".into(),         Value::from(v)); }
    if let Some(v) = args.time_bins         { params.insert("time_bins".into(),         Value::from(v)); }
    if let Some(v) = args.min_consensus     { params.insert("min_consensus".into(),     Value::from(v)); }
    if let Some(v) = args.max_events        { params.insert("max_events".into(),        Value::from(v)); }
    if let Some(v) = args.smoothing         { params.insert("smoothing".into(),         Value::from(v)); }
    if let Some(v) = args.events_per_second { params.insert("events_per_second".into(), Value::from(v)); }
    if let Some(v) = args.seed              { params.insert("seed".into(),              Value::from(v)); }
    if let Some(v) = args.bucketing         { params.insert("bucketing".into(),         Value::String(v)); }
    crate::client::call(url, "v3/project_logs", Value::Object(params))
}
