//! Semi-Markov projection of log / event sequences.
//!
//! Given a series of observed event strings, train a higher-order Markov
//! chain over event types and project the next plausible events over a
//! future look-forward window.  Three independently-tunable axes:
//!
//! 1. **State bucketing** — how raw strings get mapped to a finite state
//!    space.  Defaults to drain3 ([`crate::common::drain::DrainParser`])
//!    because it's already in the tree and produces operator-readable
//!    templates as state labels; alternatives are a built-in regex
//!    `Normalize` mode and `Identity` (each unique string is its own state).
//!
//! 2. **Order** — context size, K ∈ [1, 4].  Default K = 2.  Stupid-
//!    backoff to shorter contexts, then to the unconditional state
//!    distribution, when the full K-gram context has no observations.
//!
//! 3. **Time** — the projection is *semi-*Markov, not pure Markov: each
//!    transition draws a holding time so the projection respects the
//!    requested `duration`.  Holding times are:
//!    - the empirical observed gap distribution, when the timestamped
//!      entry-point [`markov_project_timed`] / `_with` is used; otherwise
//!    - exponential with mean `1 / events_per_second`.
//!
//! Many-rollout aggregation (operator request): the chain is rolled out
//! `n_samples` times; offsets are binned into `time_bins` buckets across
//! `[0, duration)` and we emit the modal state(s) per bucket along with
//! the share-of-samples consensus as [`ProjectedEvent::transition_prob`].
//!
//! Module-private; the public surface is the four `markov_project*`
//! free functions plus [`MarkovProjectionConfig`], [`Bucketing`], and
//! [`ProjectedEvent`].  See `analysis/mod.rs` for re-exports.

use std::collections::{HashMap, VecDeque};

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::common::drain::DrainParser;

// ─────────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────────

/// How raw input strings get mapped to a finite state space.  Drain3 is
/// the default — it produces operator-readable templates and groups
/// near-duplicate lines (e.g. `kernel: NMI on CPU 3` and `kernel: NMI
/// on CPU 7` into one state).  `Normalize` is a lightweight fallback
/// using a few regexes; `Identity` treats every distinct string as its
/// own state (useful for tests and for callers that pre-tagged events).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucketing { Drain3, Normalize, Identity }

/// Operator-tunable knobs for [`markov_project_with`] and the timed
/// sibling.  Sensible defaults via [`Default`].
#[derive(Debug, Clone)]
pub struct MarkovProjectionConfig {
    /// Markov order K — context size in events.  Clamped to `[1, 4]`.
    /// K=2 is the empirical sweet spot for log streams (captures
    /// `A → B → C` bigrams without suffering K=3 data sparsity at
    /// typical input sizes).
    pub order: u8,

    /// State bucketing strategy.  See [`Bucketing`].
    pub bucketing: Bucketing,

    /// Used by the untimed entry-point [`markov_project`] / `_with` only —
    /// rate of an assumed Poisson process for sampling holding times.
    /// The timed entry-point ([`markov_project_timed`]) uses the
    /// empirical per-transition gap distribution instead.
    pub events_per_second: f64,

    /// Hard cap on the number of [`ProjectedEvent`] entries returned.
    /// Saves callers from runaway projections when the input encodes
    /// a high-frequency loop and `duration` is long.
    pub max_events: usize,

    /// Monte Carlo rollouts.  Aggregation is done across all of them.
    pub n_samples: usize,

    /// Time-bin granularity for many-sample aggregation.  The window
    /// `[0, duration)` is divided into `time_bins` equal-width
    /// buckets; the most-frequent state(s) per bucket are emitted.
    pub time_bins: usize,

    /// Minimum share-of-samples consensus to emit a state for a bin.
    /// `0.0` includes every observed state; the default `0.10` filters
    /// to the projections that at least 10% of rollouts agreed on.
    pub min_consensus: f64,

    /// Additive Laplace-style smoothing inside a matched context — gives
    /// every state a small non-zero probability so projections retain
    /// some natural diversity instead of locking onto a pure point
    /// estimate.  Default `0.5`.
    pub smoothing: f64,

    /// Seed for the rollout RNG.  `None` (default) uses entropy.  Set
    /// to a fixed value for reproducible projections — useful for
    /// regression tests and for snapshots an operator wants to share.
    pub seed: Option<u64>,
}

impl Default for MarkovProjectionConfig {
    fn default() -> Self {
        Self {
            order:              2,
            bucketing:          Bucketing::Drain3,
            events_per_second:  1.0,
            max_events:         200,
            n_samples:          50,
            time_bins:          20,
            min_consensus:      0.10,
            smoothing:          0.5,
            seed:               None,
        }
    }
}

/// One projected event emitted by [`markov_project`] / `_with` / `_timed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectedEvent {
    /// Offset from the start of the projection window, in seconds.  The
    /// caller decides what "now" means and interprets the offset
    /// relative to that.
    pub offset_secs: f64,
    /// A concrete exemplar line from the predicted state — the
    /// first input line that mapped to this state during training.
    pub text: String,
    /// Human-readable label for the state — for `Drain3` the mined
    /// template (e.g. `kernel: NMI received on CPU <*>`), for
    /// `Normalize` the templated form, for `Identity` the literal
    /// string.
    pub source_state: String,
    /// Share of the `n_samples` rollouts that produced this state for
    /// this time-bin.  `1.0` = every rollout agreed; values near
    /// `min_consensus` are weakly supported.
    pub transition_prob: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Public entry-points (mirror the analysis/* `fn` / `fn_with` triplet)
// ─────────────────────────────────────────────────────────────────────────────

/// Convenience: project with default config.
pub fn markov_project(inputs: &[String], duration: &str) -> Vec<ProjectedEvent> {
    markov_project_with(inputs, duration, &MarkovProjectionConfig::default())
}

/// Untimed projection.  Holding times are sampled from an exponential
/// with mean `1 / cfg.events_per_second` because the input carries no
/// timestamps.  Use [`markov_project_timed_with`] when the caller has
/// real timestamps — it learns the empirical inter-arrival distribution
/// and produces much more realistic projections.
pub fn markov_project_with(
    inputs:   &[String],
    duration: &str,
    cfg:      &MarkovProjectionConfig,
) -> Vec<ProjectedEvent> {
    project_impl(inputs, /*timestamps=*/ None, duration, cfg)
}

/// Timed projection — default config.
pub fn markov_project_timed(
    events:   &[(i64, String)],
    duration: &str,
) -> Vec<ProjectedEvent> {
    markov_project_timed_with(events, duration, &MarkovProjectionConfig::default())
}

/// Timed projection.  Holding times are drawn from the empirical
/// per-transition gap distribution observed in `events`, with backoff:
/// exact `(prev → next)` → any transition into `next` → exponential
/// with mean `1 / events_per_second`.
pub fn markov_project_timed_with(
    events:   &[(i64, String)],
    duration: &str,
    cfg:      &MarkovProjectionConfig,
) -> Vec<ProjectedEvent> {
    let (timestamps, strings): (Vec<i64>, Vec<String>) = events
        .iter()
        .map(|(t, s)| (*t, s.clone()))
        .unzip();
    project_impl(&strings, Some(&timestamps), duration, cfg)
}

// ─────────────────────────────────────────────────────────────────────────────
// Implementation
// ─────────────────────────────────────────────────────────────────────────────

type StateId = u32;

fn project_impl(
    inputs:     &[String],
    timestamps: Option<&[i64]>,
    duration:   &str,
    cfg:        &MarkovProjectionConfig,
) -> Vec<ProjectedEvent> {
    // ── Validate inputs ────────────────────────────────────────────────
    if inputs.is_empty() { return Vec::new(); }
    let duration_secs = match humantime::parse_duration(duration) {
        Ok(d)  => d.as_secs_f64(),
        Err(_) => return Vec::new(),
    };
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return Vec::new();
    }
    let order = (cfg.order as usize).clamp(1, 4);
    let n_samples = cfg.n_samples.max(1);
    let time_bins = cfg.time_bins.max(1);

    // ── Bucket inputs to a finite state space ──────────────────────────
    let buckets = bucket_inputs(inputs, cfg.bucketing);
    if buckets.state_seq.is_empty() {
        return Vec::new();
    }

    // ── Train: K-gram counts + (optional) empirical gap distribution ──
    let counts = build_ngram_counts(&buckets.state_seq, order);
    let gaps   = timestamps.map(|ts| build_gap_observations(&buckets.state_seq, ts));
    let n_states = buckets.label.len();

    // ── Seed the chain with the tail of the input ─────────────────────
    let seed_context: VecDeque<StateId> = {
        let take = order.min(buckets.state_seq.len());
        buckets.state_seq[buckets.state_seq.len() - take..]
            .iter()
            .copied()
            .collect()
    };

    // ── Run N rollouts ─────────────────────────────────────────────────
    let mut rng: StdRng = match cfg.seed {
        Some(s) => StdRng::seed_from_u64(s),
        None    => StdRng::from_entropy(),
    };
    let mut samples: Vec<Vec<(f64, StateId)>> = Vec::with_capacity(n_samples);
    for _ in 0..n_samples {
        samples.push(rollout(
            seed_context.clone(),
            &counts,
            gaps.as_ref(),
            duration_secs,
            cfg,
            n_states,
            &mut rng,
        ));
    }

    // ── Aggregate across rollouts ──────────────────────────────────────
    aggregate(&samples, duration_secs, time_bins, cfg, &buckets)
}

// ─────────────────────────────────────────────────────────────────────────────
// State bucketing
// ─────────────────────────────────────────────────────────────────────────────

struct Bucketed {
    state_seq: Vec<StateId>,
    /// state_id → human-readable state label (template / templated form
    /// / literal — depends on bucketing mode)
    label:    HashMap<StateId, String>,
    /// state_id → first observed raw input line for that state
    exemplar: HashMap<StateId, String>,
}

fn bucket_inputs(inputs: &[String], mode: Bucketing) -> Bucketed {
    match mode {
        Bucketing::Drain3    => bucket_drain3(inputs),
        Bucketing::Normalize => bucket_normalize(inputs),
        Bucketing::Identity  => bucket_identity(inputs),
    }
}

fn bucket_drain3(inputs: &[String]) -> Bucketed {
    // Match the depth/sim/max-children that ShardsManager uses for
    // log-template mining elsewhere in the tree.  Re-uses the existing
    // DrainParser implementation — no duplicate templater logic.
    let mut parser = DrainParser::new(3, 0.5, 100);
    let mut state_seq: Vec<StateId> = Vec::with_capacity(inputs.len());
    let mut label:    HashMap<StateId, String> = HashMap::new();
    let mut exemplar: HashMap<StateId, String> = HashMap::new();
    for line in inputs {
        let res = parser.parse(line);
        let id = res.cluster.id as StateId;
        // Refresh the label every time — drain may have generalised
        // the template (token → `<*>`) as it ingested more lines.
        label.insert(id, res.cluster.template.join(" "));
        exemplar.entry(id).or_insert_with(|| line.clone());
        state_seq.push(id);
    }
    Bucketed { state_seq, label, exemplar }
}

fn bucket_normalize(inputs: &[String]) -> Bucketed {
    // One-shot built compiled regexes.  Heavier patterns first
    // (UUIDs before generic digit-runs) so they win on overlap.
    let uuid_re = Regex::new(
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
    ).unwrap();
    let ip_re   = Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap();
    let hex_re  = Regex::new(r"\b[0-9a-fA-F]{8,}\b").unwrap();
    let num_re  = Regex::new(r"\b\d+\b").unwrap();

    let mut to_id:    HashMap<String, StateId> = HashMap::new();
    let mut state_seq: Vec<StateId> = Vec::with_capacity(inputs.len());
    let mut label:    HashMap<StateId, String> = HashMap::new();
    let mut exemplar: HashMap<StateId, String> = HashMap::new();
    for line in inputs {
        let mut t = line.clone();
        t = uuid_re.replace_all(&t, "<UUID>").into_owned();
        t = ip_re  .replace_all(&t, "<IP>")  .into_owned();
        t = hex_re .replace_all(&t, "<HEX>") .into_owned();
        t = num_re .replace_all(&t, "<N>")   .into_owned();
        let next_id = to_id.len() as StateId;
        let id = *to_id.entry(t.clone()).or_insert(next_id);
        label.entry(id).or_insert(t);
        exemplar.entry(id).or_insert_with(|| line.clone());
        state_seq.push(id);
    }
    Bucketed { state_seq, label, exemplar }
}

fn bucket_identity(inputs: &[String]) -> Bucketed {
    let mut to_id:    HashMap<String, StateId> = HashMap::new();
    let mut state_seq: Vec<StateId> = Vec::with_capacity(inputs.len());
    let mut label:    HashMap<StateId, String> = HashMap::new();
    let mut exemplar: HashMap<StateId, String> = HashMap::new();
    for line in inputs {
        let next_id = to_id.len() as StateId;
        let id = *to_id.entry(line.clone()).or_insert(next_id);
        label.entry(id).or_insert_with(|| line.clone());
        exemplar.entry(id).or_insert_with(|| line.clone());
        state_seq.push(id);
    }
    Bucketed { state_seq, label, exemplar }
}

// ─────────────────────────────────────────────────────────────────────────────
// N-gram counts + empirical gap distribution
// ─────────────────────────────────────────────────────────────────────────────

/// `counts[k]` maps a length-`k` context to a histogram of next states.
/// `counts[0]` is the unconditional state frequency (key is `vec![]`).
type Counts = Vec<HashMap<Vec<StateId>, HashMap<StateId, u32>>>;

fn build_ngram_counts(states: &[StateId], order: usize) -> Counts {
    let mut counts: Counts = (0..=order).map(|_| HashMap::new()).collect();
    for i in 0..states.len() {
        let next = states[i];
        for k in 0..=order {
            if i >= k {
                let ctx: Vec<StateId> = states[i - k .. i].to_vec();
                *counts[k]
                    .entry(ctx)
                    .or_default()
                    .entry(next)
                    .or_insert(0) += 1;
            }
        }
    }
    counts
}

fn build_gap_observations(states: &[StateId], times: &[i64]) -> HashMap<(StateId, StateId), Vec<f64>> {
    let mut gaps: HashMap<(StateId, StateId), Vec<f64>> = HashMap::new();
    let n = states.len().min(times.len());
    for i in 1..n {
        let dt = (times[i] - times[i - 1]) as f64;
        if dt >= 0.0 {
            gaps.entry((states[i - 1], states[i])).or_default().push(dt);
        }
    }
    gaps
}

// ─────────────────────────────────────────────────────────────────────────────
// Rollout & sampling
// ─────────────────────────────────────────────────────────────────────────────

fn rollout(
    mut context:    VecDeque<StateId>,
    counts:         &Counts,
    gaps:           Option<&HashMap<(StateId, StateId), Vec<f64>>>,
    duration_secs:  f64,
    cfg:            &MarkovProjectionConfig,
    n_states:       usize,
    rng:            &mut StdRng,
) -> Vec<(f64, StateId)> {
    let mut out: Vec<(f64, StateId)> = Vec::new();
    let mut t = 0.0f64;
    let mut steps = 0usize;
    let step_cap = cfg.max_events.saturating_mul(8).max(1024);

    while t < duration_secs && steps < step_cap {
        steps += 1;
        let next = sample_next(&context, counts, n_states, cfg.smoothing, rng);
        let prev_state = context.back().copied();
        let dt = sample_gap(prev_state, next, gaps, cfg.events_per_second, rng);
        t += dt;
        if t >= duration_secs { break; }
        out.push((t, next));
        // Shift context: drop oldest, push next.  When context is
        // already at order length, this keeps it pinned to order.
        if context.len() == cfg.order as usize {
            context.pop_front();
        }
        context.push_back(next);
    }
    out
}

fn sample_next(
    context:   &VecDeque<StateId>,
    counts:    &Counts,
    n_states:  usize,
    smoothing: f64,
    rng:       &mut StdRng,
) -> StateId {
    // Try contexts of length min(K, ctx_len) down to 0, in that order
    // (longer-match-wins stupid-backoff).  Within the matched level,
    // sample with additive smoothing so unobserved states get a small
    // non-zero probability — keeps the rollout from collapsing onto
    // pure point estimates when the input has a heavy mode.
    let ctx_full: Vec<StateId> = context.iter().copied().collect();
    let max_k = ctx_full.len().min(counts.len().saturating_sub(1));
    for k in (0..=max_k).rev() {
        let start = ctx_full.len() - k;
        let ctx_key = ctx_full[start..].to_vec();
        if let Some(dist) = counts.get(k).and_then(|m| m.get(&ctx_key)) {
            // Weighted sample with α smoothing applied to OBSERVED states
            // only — adding it across the full state set would dilute
            // very heavy modes by Vec::len() at huge n_states.
            //
            // Sort by state id before sampling: HashMap iteration order
            // is non-deterministic even across separately-built maps
            // with identical contents (each instance picks its own
            // RandomState), so a stable sort here is required for the
            // `seed` knob to actually produce reproducible projections.
            let mut entries: Vec<(StateId, u32)> =
                dist.iter().map(|(s, c)| (*s, *c)).collect();
            entries.sort_by_key(|(s, _)| *s);
            let total: f64 = entries.iter().map(|(_, c)| *c as f64 + smoothing).sum();
            // `gen` is a reserved keyword in Edition 2024 — `r#gen`
            // is the escaped form.  Same applies in `sample_gap`.
            let pick = rng.r#gen::<f64>() * total;
            let mut accum = 0.0f64;
            for (state, count) in &entries {
                accum += *count as f64 + smoothing;
                if accum >= pick {
                    return *state;
                }
            }
            // Numerical edge — fall through to global fallback below.
        }
    }
    // Total fallback (shouldn't happen given counts[0] is always populated
    // when input was non-empty): uniform over all observed states.
    if n_states > 0 {
        return rng.gen_range(0..n_states as u32);
    }
    0
}

fn sample_gap(
    prev:        Option<StateId>,
    next:        StateId,
    gaps:        Option<&HashMap<(StateId, StateId), Vec<f64>>>,
    fallback_rate: f64,
    rng:         &mut StdRng,
) -> f64 {
    // Empirical-gap path (timed input).  Hard backoff:
    //   1. exact (prev → next) transition observations
    //   2. any (* → next) — "into next"
    //   3. global mean of all observed gaps
    //   4. exponential with the user-supplied rate
    if let (Some(gmap), Some(p)) = (gaps, prev) {
        if let Some(obs) = gmap.get(&(p, next)) {
            if !obs.is_empty() {
                return obs[rng.gen_range(0..obs.len())];
            }
        }
        // Fallback lists are flattened from HashMap iteration order,
        // which is non-deterministic across runs (each HashMap picks
        // its own RandomState).  Sort the flattened gaps so the
        // `seed` knob produces reproducible projections — the
        // sampled distribution is unchanged.
        let mut into_next: Vec<f64> = gmap.iter()
            .filter(|((_, n), _)| *n == next)
            .flat_map(|(_, v)| v.iter().copied())
            .collect();
        into_next.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if !into_next.is_empty() {
            return into_next[rng.gen_range(0..into_next.len())];
        }
        let mut all: Vec<f64> = gmap.values().flat_map(|v| v.iter().copied()).collect();
        all.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if !all.is_empty() {
            return all[rng.gen_range(0..all.len())];
        }
    }
    // Exponential with mean 1/rate; rate floored to avoid div-by-zero
    // and to avoid pathologically long gaps that would never produce
    // any event within `duration`.
    let rate = fallback_rate.max(0.001);
    let u: f64 = rng.r#gen::<f64>().max(f64::MIN_POSITIVE);
    -u.ln() / rate
}

// ─────────────────────────────────────────────────────────────────────────────
// Many-sample aggregation
// ─────────────────────────────────────────────────────────────────────────────

fn aggregate(
    samples:       &[Vec<(f64, StateId)>],
    duration_secs: f64,
    time_bins:     usize,
    cfg:           &MarkovProjectionConfig,
    buckets:       &Bucketed,
) -> Vec<ProjectedEvent> {
    let bin_width = duration_secs / time_bins as f64;
    let n_samples = samples.len().max(1) as f64;

    // bin_votes[bin] : state_id → count of rollouts that emitted that
    // state in that time bucket.  An event is counted at most once per
    // (sample, bin) pair so the share-of-samples interpretation holds
    // even when a single rollout produces multiple events in one bin
    // (a tight loop in the chain).
    let mut bin_votes: Vec<HashMap<StateId, u32>> =
        (0..time_bins).map(|_| HashMap::new()).collect();
    for sample in samples {
        let mut seen_in_bin: HashMap<(usize, StateId), ()> = HashMap::new();
        for &(offset, state) in sample {
            let bin = (offset / bin_width).floor().clamp(0.0, (time_bins - 1) as f64) as usize;
            if seen_in_bin.insert((bin, state), ()).is_none() {
                *bin_votes[bin].entry(state).or_insert(0) += 1;
            }
        }
    }

    let mut out: Vec<ProjectedEvent> = Vec::new();
    for (b, votes) in bin_votes.iter().enumerate() {
        let mut sorted: Vec<(StateId, u32)> =
            votes.iter().map(|(s, c)| (*s, *c)).collect();
        // Sort by vote-count descending; tie-break by state id ascending
        // so ties between equally-supported states resolve deterministically
        // (HashMap iteration order is non-deterministic across runs).
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let bin_midpoint = (b as f64 + 0.5) * bin_width;
        for (state, count) in sorted {
            let prob = count as f64 / n_samples;
            if prob < cfg.min_consensus { break; }
            out.push(ProjectedEvent {
                offset_secs:     bin_midpoint,
                text:            buckets.exemplar.get(&state).cloned().unwrap_or_default(),
                source_state:    buckets.label.get(&state).cloned().unwrap_or_default(),
                transition_prob: prob,
            });
            if out.len() >= cfg.max_events { return out; }
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_seeded(seed: u64) -> MarkovProjectionConfig {
        MarkovProjectionConfig {
            seed: Some(seed),
            n_samples: 50,
            ..Default::default()
        }
    }

    #[test]
    fn empty_input_returns_empty() {
        let out = markov_project(&[], "1h");
        assert!(out.is_empty());
    }

    #[test]
    fn bad_duration_returns_empty() {
        let out = markov_project(&["x".to_owned(), "y".to_owned()], "garbage");
        assert!(out.is_empty());
    }

    #[test]
    fn zero_duration_returns_empty() {
        let out = markov_project(&["x".to_owned(), "y".to_owned()], "0s");
        assert!(out.is_empty());
    }

    #[test]
    fn seed_determinism() {
        let inputs = vec!["a".to_owned(), "b".to_owned(), "a".to_owned(), "c".to_owned(),
                          "a".to_owned(), "b".to_owned(), "a".to_owned(), "c".to_owned()];
        let cfg = cfg_seeded(42);
        let a = markov_project_with(&inputs, "30s", &cfg);
        let b = markov_project_with(&inputs, "30s", &cfg);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.source_state, y.source_state);
            assert!((x.offset_secs - y.offset_secs).abs() < 1e-9);
            assert!((x.transition_prob - y.transition_prob).abs() < 1e-9);
        }
    }

    #[test]
    fn respects_duration_window() {
        let inputs: Vec<String> = (0..20).map(|i| format!("event {i}")).collect();
        let cfg = MarkovProjectionConfig {
            seed: Some(7),
            events_per_second: 5.0,  // many events in 10s
            ..Default::default()
        };
        let out = markov_project_with(&inputs, "10s", &cfg);
        for e in &out {
            assert!(e.offset_secs <= 10.0 + 1e-6, "offset {} > 10s", e.offset_secs);
        }
    }

    #[test]
    fn drain3_groups_near_duplicates() {
        // Same template, different numbers — drain3 should collapse them
        // into one cluster.  Output should re-emit the same cluster.
        let inputs: Vec<String> = (1..=30)
            .map(|i| format!("kernel: NMI received on CPU {i}"))
            .collect();
        let cfg = MarkovProjectionConfig {
            seed: Some(13),
            bucketing: Bucketing::Drain3,
            ..Default::default()
        };
        let out = markov_project_with(&inputs, "10s", &cfg);
        assert!(!out.is_empty());
        // Every projected event should have come from the single drain
        // cluster the input produces.
        let unique_states: std::collections::HashSet<&String> =
            out.iter().map(|e| &e.source_state).collect();
        assert_eq!(unique_states.len(), 1, "expected one drain state, got {unique_states:?}");
    }

    #[test]
    fn normalize_collapses_digits() {
        let inputs = vec![
            "req 1 ok".to_owned(),
            "req 2 ok".to_owned(),
            "req 9001 ok".to_owned(),
        ];
        let cfg = MarkovProjectionConfig {
            seed: Some(1),
            bucketing: Bucketing::Normalize,
            ..Default::default()
        };
        let out = markov_project_with(&inputs, "5s", &cfg);
        let unique_states: std::collections::HashSet<&String> =
            out.iter().map(|e| &e.source_state).collect();
        assert_eq!(unique_states.len(), 1, "all three should collapse to one state");
        let label = unique_states.iter().next().unwrap();
        assert!(label.contains("<N>"), "label should carry the <N> placeholder: {label}");
    }

    #[test]
    fn identity_treats_each_string_as_own_state() {
        // With identity bucketing on an alternating sequence, the chain
        // should learn the A↔B alternation perfectly at K=2.
        let inputs: Vec<String> = (0..20)
            .map(|i| if i % 2 == 0 { "alpha" } else { "beta" }.to_owned())
            .collect();
        let cfg = MarkovProjectionConfig {
            seed: Some(5),
            bucketing: Bucketing::Identity,
            events_per_second: 5.0,
            time_bins: 10,
            min_consensus: 0.5,
            ..Default::default()
        };
        let out = markov_project_with(&inputs, "5s", &cfg);
        // The dominant projection at every bin should be one of the two
        // observed states — never anything else.
        for e in &out {
            assert!(e.source_state == "alpha" || e.source_state == "beta",
                "unexpected state {}", e.source_state);
        }
    }

    #[test]
    fn single_state_input_stays_in_state() {
        let inputs: Vec<String> = vec!["only".to_owned(); 10];
        let cfg = MarkovProjectionConfig {
            seed: Some(2),
            bucketing: Bucketing::Identity,
            events_per_second: 4.0,
            ..Default::default()
        };
        let out = markov_project_with(&inputs, "5s", &cfg);
        assert!(!out.is_empty());
        for e in &out {
            assert_eq!(e.source_state, "only");
        }
    }

    #[test]
    fn timed_variant_uses_empirical_gaps() {
        // Build an alternating sequence with ALWAYS-10s gaps.  The timed
        // projection should produce events spaced ~10s apart, regardless
        // of cfg.events_per_second (which the timed path falls back to
        // only when no observation matches).
        let events: Vec<(i64, String)> = (0..10)
            .map(|i| (i * 10, if i % 2 == 0 { "alpha" } else { "beta" }.to_owned()))
            .collect();
        let cfg = MarkovProjectionConfig {
            seed: Some(11),
            bucketing: Bucketing::Identity,
            events_per_second: 100.0, // would imply 100ms gaps if used
            time_bins: 60,
            min_consensus: 0.5,
            ..Default::default()
        };
        let out = markov_project_timed_with(&events, "60s", &cfg);
        assert!(!out.is_empty());
        // The first bin (0–1s) should be empty (or have only a single
        // late-tail event) — definitely not a tight 100ms-gap burst.
        // A reasonable bound: there are at most 7 events in 60s for a
        // 10s-gap chain (some MC samples will have 5, some 7).
        assert!(out.len() <= 7 * /* states per bin upper bound */ 2,
            "got {} events; expected ~6 with 10s empirical gaps", out.len());
    }

    #[test]
    fn min_consensus_filters_low_support() {
        // High consensus threshold should drop low-vote states even when
        // they exist in some rollouts.
        let inputs: Vec<String> = (0..50)
            .map(|i| format!("event {}", i % 5))
            .collect();
        let strict = MarkovProjectionConfig {
            seed: Some(3),
            bucketing: Bucketing::Identity,
            min_consensus: 0.95,
            n_samples: 30,
            ..Default::default()
        };
        let lax = MarkovProjectionConfig { min_consensus: 0.0, ..strict.clone() };
        let strict_out = markov_project_with(&inputs, "20s", &strict);
        let lax_out    = markov_project_with(&inputs, "20s", &lax);
        // Strict produces a subset of lax (could be empty if no bin
        // achieves 95% consensus, which is fine).
        assert!(strict_out.len() <= lax_out.len());
    }

    #[test]
    fn max_events_caps_output() {
        let inputs: Vec<String> = (0..100).map(|i| format!("e{}", i % 10)).collect();
        let cfg = MarkovProjectionConfig {
            seed: Some(99),
            bucketing: Bucketing::Identity,
            events_per_second: 50.0,
            max_events: 3,
            min_consensus: 0.0,
            time_bins: 200,
            ..Default::default()
        };
        let out = markov_project_with(&inputs, "60s", &cfg);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn projected_events_are_time_ordered() {
        let inputs: Vec<String> = (0..30).map(|i| format!("e{}", i % 4)).collect();
        let cfg = MarkovProjectionConfig {
            seed: Some(8),
            bucketing: Bucketing::Identity,
            events_per_second: 4.0,
            min_consensus: 0.0,
            ..Default::default()
        };
        let out = markov_project_with(&inputs, "30s", &cfg);
        for w in out.windows(2) {
            assert!(w[0].offset_secs <= w[1].offset_secs,
                "events out of order: {:?} then {:?}", w[0].offset_secs, w[1].offset_secs);
        }
    }
}
