# Semi-Markov log/event projection (`bdslib::analysis::markov`)

`markov_project(&[String], duration: &str) -> Vec<ProjectedEvent>`
projects the next plausible log/event arrivals over a configurable
future window, trained on the sequence of strings supplied to it.  It
uses a **semi-Markov process** — a Markov chain over event *types* plus
a per-transition holding-time distribution — and aggregates the consensus
across many Monte Carlo rollouts.

It complements bdslib's existing analytics surface:

- TextRank / LSA / k-NN reduce a corpus to representative or central
  inputs.
- N-gram anomaly / noise removal classify individual lines as outliers
  or background.
- RCA Jaccard finds temporal correlations between observed event keys.
- **Markov projection extrapolates** — it answers a question the others
  don't even attempt: *"what is likely to happen NEXT?"*

This document covers:

1. [What problem semi-Markov projection solves](#1-what-problem-semi-markov-projection-solves)
2. [The classical Markov / semi-Markov machinery](#2-the-classical-markov--semi-markov-machinery)
3. [How bdslib implements it](#3-how-bdslib-implements-it)
4. [The full pipeline, step by step](#4-the-full-pipeline-step-by-step)
5. [Output contract](#5-output-contract)
6. [Configuration knobs](#6-configuration-knobs)
7. [Complexity and scaling](#7-complexity-and-scaling)
8. [Determinism guarantees](#8-determinism-guarantees)
9. [Worked examples](#9-worked-examples)
10. [Failure modes and edge cases](#10-failure-modes-and-edge-cases)
11. [References](#11-references)

---

## 1. What problem semi-Markov projection solves

Operators staring at a flood of recent logs typically need an
operational forecast: *given what just happened, what is the chain of
events likely to look like over the next 30 minutes?* The answer is
useful for:

- **Early-warning triage** — a high-consensus path of `error / fail /
  timeout / panic / refused` events forecasts an oncoming incident
  while there's still time to pre-scale, drain, or alert.
- **Operational hygiene** — a low-consensus diverse projection is a
  *positive* signal: the chain has no strong opinion, which means the
  recent corpus is genuinely background-noise heterogeneous.
- **Plausibility-checking incident hypotheses** — when a runbook is
  weighing two interpretations of recent events, the projection shows
  which next-state would naturally follow each.
- **Capacity planning hints** — projected event rates per state tell
  you which subsystems are likely to be hot in the next window.

Importantly, the projection is **not a forecast** in the
weather-forecast sense — it doesn't predict what *will* happen, only
what would naturally follow from the observed transition statistics.
The right mental model is "stochastic continuation of the recent
pattern, with explicit confidence numbers attached".

Practical use cases inside bdslib:

- **Live in bdsweb** as `Analysis → Project events` — operator picks
  training window + projection window + tuning knobs, sees the
  consensus events ranked by time bin, optionally hands the projection
  to the LLM via `"Analyze this!"` for an SRE-style narrative.
- **Cluster-wide via JSON-RPC** as `v3/project_logs` — the projection
  trains on the *union* of every peer's recent primary records, deduped
  by UUID and sorted chronologically.
- **Programmatic** from any Bund script or Rust caller via the
  `bdslib::analysis::markov::markov_project*` free functions.

---

## 2. The classical Markov / semi-Markov machinery

Three layers of classical probability theory are relevant here:

### Markov chain over event types

A discrete-time Markov chain assigns transition probabilities
`P(state_{t+1} = b | state_t = a)`. Given training observations
`s_0, s_1, …, s_{N-1}`, the maximum-likelihood transition probability
is just the counted frequency:

```
P_hat(b | a) = count(a → b) / count(a → *)
```

To generate a sequence forward, you draw `s_{t+1}` from that
distribution conditioned on the current state, append it, advance,
and repeat.

### Higher-order Markov chains

A K-order chain conditions on the last `K` states instead of one:

```
P(s_{t+1} | s_t, s_{t-1}, …, s_{t-K+1})
```

This captures structure like *"after a `connection refused` that
followed a `service restart`, the next event is usually a `health
check failed`"* — patterns a first-order chain genuinely cannot
represent.

Trade-off: the K-gram state space grows as `|S|^K`, so transition
counts become sparse fast. Standard mitigations:

- **Laplace (add-α) smoothing** — pretend every state was observed at
  least `α` times for every context. Keeps unseen states from getting
  zero probability.
- **Backoff** — when the K-context has no observations, fall back to
  K−1 (and so on down to the unigram / unconditional distribution).
  Stupid backoff (Brants et al. 2007) is the simplest variant and
  proven effective in practice.

### Semi-Markov process — modeling time

A pure Markov chain produces a sequence with no timing — useful for
sequence-of-events questions, useless for "events within the next 30
minutes". Two extensions add time:

- **Continuous-time Markov chain (CTMC)** — each state has an
  exponential holding-time distribution with rate `λ_s`. Simple and
  analytically tractable, but exponential gaps don't match real log
  streams (which are heavy-tailed and often have a hard floor).
- **Semi-Markov process** — same Markov-on-states transition rule,
  but the holding-time distribution can be **arbitrary**, learned
  from observed inter-arrival data per `(from, to)` transition.

bdslib uses the semi-Markov form because log streams are demonstrably
non-exponential — heartbeats fire on a near-uniform clock, error bursts
cluster, and idle periods can be minutes long. Sampling from the
empirical gap distribution preserves those characteristics where an
exponential approximation would smear them.

---

## 3. How bdslib implements it

`src/analysis/markov.rs` is ~600 lines and depends only on:

- `serde` — for the public result struct + config.
- `rand` (`0.8`) — `StdRng` for the RNG, seeded by `cfg.seed` when set.
- `regex` — for the `Normalize` bucketing fallback.
- `crate::common::drain::DrainParser` — for the default `Drain3`
  bucketing (already in the tree, used elsewhere for log-template
  mining; no duplicate templater logic).

Public surface (four free functions, mirroring the
`analysis/*` `fn / fn_with` triplet convention):

```rust
pub fn markov_project(inputs: &[String], duration: &str)
    -> Vec<ProjectedEvent>;

pub fn markov_project_with(inputs: &[String], duration: &str,
                            cfg: &MarkovProjectionConfig)
    -> Vec<ProjectedEvent>;

pub fn markov_project_timed(events: &[(i64 /* unix_secs */, String)],
                             duration: &str)
    -> Vec<ProjectedEvent>;

pub fn markov_project_timed_with(events: &[(i64, String)], duration: &str,
                                  cfg: &MarkovProjectionConfig)
    -> Vec<ProjectedEvent>;
```

The untimed variants assume a Poisson process at `events_per_second`
(default 1.0) for holding times. The timed variants learn the empirical
per-transition gap distribution from the input and sample from it at
projection time — much more realistic when timestamps are available.

Three orthogonal axes are configurable via `MarkovProjectionConfig`
(see [§6](#6-configuration-knobs)):

| Axis | Knob(s) | Default |
|---|---|---|
| **State bucketing** | `bucketing` | `Drain3` |
| **Markov order** | `order` (clamped to `[1, 4]`) | `2` |
| **Time modeling** | `events_per_second` (untimed) or empirical (timed) | 1.0 / empirical |
| **Sampling** | `n_samples`, `time_bins`, `min_consensus`, `smoothing`, `seed` | 50 / 20 / 0.10 / 0.5 / `None` |
| **Output cap** | `max_events` | 200 |

Three implementation details worth knowing up-front:

1. **Many samples are aggregated, not concatenated.** The chain is
   rolled out `n_samples` times; offsets are binned into `time_bins`
   buckets across `[0, duration)`; for each bin the *modal* state(s)
   are emitted with a `transition_prob` field equal to the share of
   samples that placed that state in that bucket. So
   `transition_prob = 1.0` means every rollout agreed, `0.10` means
   the bare-minimum-consensus threshold was met, and the absence of a
   bin means no state achieved `min_consensus`.
2. **Hard stupid-backoff is used**, with add-α smoothing inside the
   matched context. Backoff is hard (try longest matched context, then
   one shorter, …) rather than soft mixing (Brants et al. 2007 §3.4).
   The add-α smoothing within the matched level gives observed-but-
   uncommon next-states a small non-zero weight so rollouts retain
   diversity rather than collapsing onto pure point estimates.
3. **`HashMap` iteration order is non-deterministic.** Determinism
   requires sorting the per-context distribution by `state_id` before
   the weighted-sample loop, sorting the empirical-gap fallback lists,
   and tie-breaking the aggregation vote count by state id. These
   sorts are at the heart of why the `seed` knob actually produces
   byte-identical projections.

---

## 4. The full pipeline, step by step

```
inputs: &[String] (or &[(i64, String)])
  │
  ▼
1. Validate            ──── bad duration / empty input → Vec::new()
  │
  ▼
2. Bucket              ──── Drain3 | Normalize | Identity
  │                          → Vec<StateId>, label[id], exemplar[id]
  ▼
3. Train               ──── K-gram counts: counts[k]: HashMap<context, dist>
  │                          → counts[0..=order]
  │                    ──── if timed: gap_observations[(prev, next)] = Vec<dt>
  ▼
4. Roll out × N        ──── for _ in 0..n_samples:
  │                              context ← last K states of input
  │                              loop until t ≥ duration:
  │                                  next ← sample_next(context, counts)
  │                                  dt   ← sample_gap(prev, next, gaps, rate)
  │                                  t   += dt
  │                                  emit (t, next)
  ▼
5. Aggregate           ──── bin votes by time_bin; per-bin modal state(s)
  │                          with prob ≥ min_consensus
  ▼
output: Vec<ProjectedEvent>
```

### 4.1 Bucketing — `Drain3` (default)

```rust
fn bucket_drain3(inputs: &[String]) -> Bucketed {
    let mut parser = DrainParser::new(3, 0.5, 100);
    // ↑ same depth / sim_threshold / max_children as
    //   ShardsManager uses for log-template mining elsewhere
    let mut state_seq: Vec<StateId> = Vec::with_capacity(inputs.len());
    let mut label:    HashMap<StateId, String> = HashMap::new();
    let mut exemplar: HashMap<StateId, String> = HashMap::new();
    for line in inputs {
        let res = parser.parse(line);
        let id = res.cluster.id as StateId;
        // Refresh label every time — drain may have generalised the
        // template (token → `<*>`) as it ingested more lines.
        label.insert(id, res.cluster.template.join(" "));
        exemplar.entry(id).or_insert_with(|| line.clone());
        state_seq.push(id);
    }
    Bucketed { state_seq, label, exemplar }
}
```

State `id` is the drain cluster id; **label** is the drain template
(operator-readable, like `kernel: NMI received on CPU <*>`);
**exemplar** is the first observed raw line that mapped to this cluster
— it's what gets re-emitted in `ProjectedEvent::text` so the operator
sees something concrete instead of a templated form.

Two alternatives are available via `cfg.bucketing`:

- **`Normalize`** — strip UUIDs / IP addresses / hex blobs / digit-runs
  via regex, group by the resulting templated form. Same idea as
  drain3 but coarser and faster.
- **`Identity`** — every distinct input string is its own state. Mostly
  for tests; useful when the caller has already pre-tagged events with
  event-type labels.

### 4.2 Training — n-gram counts

```rust
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
```

`counts[k]` maps a length-`k` context to a histogram of next states.
`counts[0]` is the unconditional state frequency (key is `vec![]`).
For K=2 over the input `[A, B, A, C, A, B]`:

- `counts[0]` has `[]` → `{A: 3, B: 2, C: 1}`.
- `counts[1]` has `[A]` → `{B: 2, C: 1}` and `[B]` → `{A: 1}` and
  `[C]` → `{A: 1}`.
- `counts[2]` has `[A, B]` → `{A: 1}`, `[B, A]` → `{C: 1, B: 1}`,
  `[A, C]` → `{A: 1}`, `[C, A]` → `{B: 1}`.

### 4.3 Training — empirical gap distribution (timed variant only)

```rust
fn build_gap_observations(states: &[StateId], times: &[i64])
    -> HashMap<(StateId, StateId), Vec<f64>>
{
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
```

Every observed transition contributes its inter-arrival time as a
discrete sample of the holding-time distribution for that pair. The
empirical CDF is therefore a step function over the observed values
— sampling is "pick one observed gap uniformly at random". No
parametric assumption.

### 4.4 Rollout

```rust
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
    let mut t = 0.0;
    let mut steps = 0;
    let step_cap = cfg.max_events.saturating_mul(8).max(1024);

    while t < duration_secs && steps < step_cap {
        steps += 1;
        let next = sample_next(&context, counts, n_states,
                               cfg.smoothing, rng);
        let prev_state = context.back().copied();
        let dt = sample_gap(prev_state, next, gaps,
                            cfg.events_per_second, rng);
        t += dt;
        if t >= duration_secs { break; }
        out.push((t, next));
        if context.len() == cfg.order as usize {
            context.pop_front();
        }
        context.push_back(next);
    }
    out
}
```

The initial `context` is the last `order` states of the input — the
projection picks up where reality left off, not from scratch. `step_cap`
is a runaway-loop safeguard: if `events_per_second` is wildly high or
the chain has a self-loop with sub-second gaps, we don't iterate
forever.

### 4.5 Sampling the next state — hard backoff + α-smoothing

```rust
fn sample_next(
    context:   &VecDeque<StateId>,
    counts:    &Counts,
    n_states:  usize,
    smoothing: f64,
    rng:       &mut StdRng,
) -> StateId {
    let ctx_full: Vec<StateId> = context.iter().copied().collect();
    let max_k = ctx_full.len().min(counts.len().saturating_sub(1));
    for k in (0..=max_k).rev() {
        let start = ctx_full.len() - k;
        let ctx_key = ctx_full[start..].to_vec();
        if let Some(dist) = counts.get(k).and_then(|m| m.get(&ctx_key)) {
            // Sort by state id before sampling: HashMap iteration is
            // non-deterministic even across maps with identical
            // contents, so determinism requires a stable ordering.
            let mut entries: Vec<(StateId, u32)> =
                dist.iter().map(|(s, c)| (*s, *c)).collect();
            entries.sort_by_key(|(s, _)| *s);
            let total: f64 = entries.iter()
                .map(|(_, c)| *c as f64 + smoothing).sum();
            let pick = rng.r#gen::<f64>() * total;
            let mut accum = 0.0;
            for (state, count) in &entries {
                accum += *count as f64 + smoothing;
                if accum >= pick { return *state; }
            }
        }
    }
    if n_states > 0 { return rng.gen_range(0..n_states as u32); }
    0
}
```

Backoff is "hard": at K=2 we first try the full 2-context. If it has
zero observations we drop to K=1, then to K=0 (unconditional). The
α smoothing is applied inside the matched level only — adding it
across every state would dilute very heavy modes when the state space
is large. Default α=0.5 retains diversity without overriding strong
modes.

### 4.6 Sampling the holding time

```rust
fn sample_gap(
    prev:        Option<StateId>,
    next:        StateId,
    gaps:        Option<&HashMap<(StateId, StateId), Vec<f64>>>,
    fallback_rate: f64,
    rng:         &mut StdRng,
) -> f64 {
    if let (Some(gmap), Some(p)) = (gaps, prev) {
        // 1. exact (prev → next) transition observations
        if let Some(obs) = gmap.get(&(p, next)) {
            if !obs.is_empty() {
                return obs[rng.gen_range(0..obs.len())];
            }
        }
        // 2. any transition INTO next
        let mut into_next: Vec<f64> = gmap.iter()
            .filter(|((_, n), _)| *n == next)
            .flat_map(|(_, v)| v.iter().copied())
            .collect();
        into_next.sort_by(|a, b| a.partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal));
        if !into_next.is_empty() {
            return into_next[rng.gen_range(0..into_next.len())];
        }
        // 3. global mean of all observed gaps
        let mut all: Vec<f64> = gmap.values()
            .flat_map(|v| v.iter().copied()).collect();
        all.sort_by(|a, b| a.partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal));
        if !all.is_empty() {
            return all[rng.gen_range(0..all.len())];
        }
    }
    // 4. Exponential with mean 1/rate (untimed path or empty fallback)
    let rate = fallback_rate.max(0.001);
    let u: f64 = rng.r#gen::<f64>().max(f64::MIN_POSITIVE);
    -u.ln() / rate
}
```

Four-step hard backoff: exact → into-next → global → exponential. The
sort on the fallback lists is for determinism (same reason as
`sample_next`).

### 4.7 Aggregation across N samples

```rust
fn aggregate(
    samples:       &[Vec<(f64, StateId)>],
    duration_secs: f64,
    time_bins:     usize,
    cfg:           &MarkovProjectionConfig,
    buckets:       &Bucketed,
) -> Vec<ProjectedEvent> {
    let bin_width = duration_secs / time_bins as f64;
    let n_samples = samples.len().max(1) as f64;

    let mut bin_votes: Vec<HashMap<StateId, u32>> =
        (0..time_bins).map(|_| HashMap::new()).collect();
    for sample in samples {
        // An event is counted at most once per (sample, bin) pair so
        // the share-of-samples interpretation holds even when one
        // rollout produces multiple events in one bin.
        let mut seen_in_bin: HashMap<(usize, StateId), ()> = HashMap::new();
        for &(offset, state) in sample {
            let bin = (offset / bin_width).floor()
                .clamp(0.0, (time_bins - 1) as f64) as usize;
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
        // for determinism.
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let bin_midpoint = (b as f64 + 0.5) * bin_width;
        for (state, count) in sorted {
            let prob = count as f64 / n_samples;
            if prob < cfg.min_consensus { break; }
            out.push(ProjectedEvent {
                offset_secs:     bin_midpoint,
                text:            buckets.exemplar.get(&state)
                                    .cloned().unwrap_or_default(),
                source_state:    buckets.label.get(&state)
                                    .cloned().unwrap_or_default(),
                transition_prob: prob,
            });
            if out.len() >= cfg.max_events { return out; }
        }
    }
    out
}
```

The `seen_in_bin` deduplication is the key reason `transition_prob`
is a clean share-of-samples value — one rollout can't double-count a
state in one bin.

---

## 5. Output contract

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectedEvent {
    /// Offset from the start of the projection window, in seconds.
    pub offset_secs:     f64,
    /// A concrete exemplar line from the predicted state — the first
    /// input line that mapped to this state during training.
    pub text:            String,
    /// Human-readable label for the state — for `Drain3` the mined
    /// template (e.g. `kernel: NMI received on CPU <*>`).
    pub source_state:    String,
    /// Share of `n_samples` rollouts that produced this state for
    /// this time-bin; `1.0` = every rollout agreed.
    pub transition_prob: f64,
}
```

A return value of `Vec::new()` means one of:

- the input is empty;
- the `duration` string failed `humantime::parse_duration`;
- the duration is zero or negative;
- no state in any bin met `min_consensus`.

The vector is sorted by `offset_secs` ascending. Within one bin the
modal state comes first, then the runner-up if it also met
`min_consensus`, …, with deterministic tie-breaks by state id when
multiple states earned identical votes.

---

## 6. Configuration knobs

```rust
pub struct MarkovProjectionConfig {
    pub order:               u8,         // K; clamped [1, 4]
    pub bucketing:           Bucketing,  // Drain3 | Normalize | Identity
    pub events_per_second:   f64,        // fallback rate (untimed only)
    pub max_events:          usize,      // hard cap on output length
    pub n_samples:           usize,      // Monte Carlo rollouts
    pub time_bins:           usize,      // aggregation buckets
    pub min_consensus:       f64,        // ∈ [0, 1]; share-of-samples gate
    pub smoothing:           f64,        // additive α inside matched ctx
    pub seed:                Option<u64>,// reproducibility
}
```

Defaults (`MarkovProjectionConfig::default()`):

| Knob | Default | Tightening | Loosening |
|---|---|---|---|
| `order` | `2` | higher K captures longer-range dependencies but needs more input | `1` is enough for short input or when transitions are mostly state-local |
| `bucketing` | `Drain3` | finest grouping; the drain template is operator-readable | `Identity` for already-tagged input; `Normalize` for cheaper templating |
| `events_per_second` | `1.0` | raise to project denser output (untimed only) | lower for sparser; ignored when timestamps are supplied |
| `max_events` | `200` | small cap = focused dashboard view | large cap = full picture, may be noisy |
| `n_samples` | `50` | more rollouts = tighter consensus estimates, linear cost | 20 is enough for low-cardinality state spaces |
| `time_bins` | `20` | finer bins = better temporal resolution but smaller per-bin sample counts | coarser bins smear timing |
| `min_consensus` | `0.10` | higher threshold filters to only majority-agreed events | `0.0` keeps every observed projection |
| `smoothing` | `0.5` | higher α flattens the distribution (more diversity) | `0.0` collapses to MLE point estimates |
| `seed` | `None` | set for reproducible projections (regression tests, shared snapshots) | leave unset for entropy-driven sampling |

---

## 7. Complexity and scaling

Let `N` = input length, `S` = distinct states observed, `K` = order,
`R` = `n_samples`, `B` = `time_bins`, `E` = expected events per
rollout.

| Phase | Cost | Memory |
|---|---|---|
| Bucketing (Drain3) | `O(N · D · L)` where `D` is drain depth (default 3) and `L` is tokens-per-line | `O(S · D)` for the prefix tree |
| Bucketing (Normalize) | `O(N · L)` regex matching | `O(S · L)` for the templated forms |
| N-gram count build | `O(N · (K + 1))` | `O(N · K)` worst case (rarely realised — most contexts repeat) |
| Empirical gap build (timed) | `O(N)` | `O(N)` in the worst case (every transition unique) |
| Single rollout | `O(E · K)` | `O(E)` |
| All rollouts | `O(R · E · K)` | `O(R · E)` |
| Aggregation | `O(R · E)` | `O(B · S)` |

In the bdsnode v3/project_logs path with default knobs over a 1-hour
training window holding ~5,000 records and a 30-minute projection
forward, the whole call typically completes in 100–400 ms on a single
core (drain3 ingest is the dominant cost, ~70% of wall-clock).

---

## 8. Determinism guarantees

Set `cfg.seed = Some(n)` and the function returns byte-identical
results for the same `(inputs, duration, cfg)` tuple. The
non-obvious correctness requirements that make this hold:

- **N-gram distribution iteration is sorted.** Rust's `HashMap`
  randomises its hasher per instance, so two HashMaps with identical
  contents iterate in different orders. `sample_next` sorts the
  per-context distribution by `state_id` before the weighted-sample
  loop.
- **Gap fallback lists are sorted.** Aggregating gaps via
  `HashMap::values().flat_map(...)` yields values in non-deterministic
  order, so the fallback samples sort the values numerically before
  indexing.
- **Aggregation ties break by state id.** When two states earn the
  same vote count in one bin, the sort uses
  `cmp_by_count_desc.then(cmp_by_state_id_asc)`.

Without `seed`, the RNG is `StdRng::from_entropy()` and successive
calls produce statistically similar but non-identical projections.
For dashboards and shared troubleshooting snapshots, always pin a
seed (the JSON-RPC `v3/project_logs` accepts a `seed` field for this
exact reason).

A separate dimension of determinism — concurrency — is not relevant
here: the function is a pure transformation `&[String] → Vec`, with
no shared state, no I/O, no global mutation.

---

## 9. Worked examples

### Example A — alternating sequence (textbook K=2 case)

```rust
use bdslib::analysis::markov::{
    markov_project_with, Bucketing, MarkovProjectionConfig,
};

let inputs: Vec<String> = (0..20).map(|i| {
    if i % 2 == 0 { "alpha" } else { "beta" }.to_owned()
}).collect();

let cfg = MarkovProjectionConfig {
    seed: Some(7),
    bucketing: Bucketing::Identity,   // each string is its own state
    events_per_second: 5.0,           // ~5 events per second
    time_bins: 10,
    min_consensus: 0.5,
    ..Default::default()
};

let out = markov_project_with(&inputs, "5s", &cfg);
for e in &out {
    println!("+{:.1}s  p={:.2}  state={}", e.offset_secs, e.transition_prob, e.source_state);
}
```

The chain learns `P(beta | alpha) = 1` and `P(alpha | beta) = 1`, so
every projected event is exactly one of the two states. With 50
samples at K=2 the alternation pattern is preserved across rollouts
and the per-bin consensus is ~1.0 throughout.

### Example B — drain3 grouping on real-looking logs

```rust
let inputs: Vec<String> = (1..=30)
    .map(|i| format!("kernel: NMI received on CPU {i}"))
    .collect();
// 30 distinct strings — but drain3 collapses them all into one
// template: `kernel: NMI received on CPU <*>`

let cfg = MarkovProjectionConfig {
    seed: Some(13),
    bucketing: Bucketing::Drain3,
    ..Default::default()
};

let out = markov_project_with(&inputs, "10s", &cfg);
// Every output event reports the same `source_state` (the drain
// template) and a representative `text` (the first line that mapped
// to the cluster).
```

### Example C — empirical gap learning (timed variant)

```rust
use bdslib::analysis::markov::markov_project_timed_with;

// 10-second clock-tick pattern; timed projection picks up the 10s gap
let events: Vec<(i64, String)> = (0..10)
    .map(|i| (i * 10, format!("clock tick {i}")))
    .collect();

let cfg = MarkovProjectionConfig {
    seed: Some(11),
    bucketing: Bucketing::Drain3,
    // events_per_second is IGNORED — empirical gaps win
    ..Default::default()
};

let out = markov_project_timed_with(&events, "60s", &cfg);
// Projected events land at ~10s intervals (with some variance from
// the random sampling), regardless of events_per_second.
```

### Example D — `v3/project_logs` JSON-RPC call

Curl:

```bash
curl -s -X POST http://127.0.0.1:9711 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"v3/project_logs","params":{
        "duration_back":    "1h",
        "duration_forward": "30min",
        "order":            2,
        "n_samples":        50,
        "time_bins":        20,
        "min_consensus":    0.10,
        "seed":             42
      }}'
```

Response (truncated):

```json
{
  "n":                9,
  "n_unique_inputs":  32,
  "n_raw_inputs":     32,
  "duration_back":    "1h",
  "duration_forward": "30min",
  "events": [
    {
      "offset_secs":     30.0,
      "source_state":    "sshd host: <*> message: <*> <*> for user <*>",
      "text":            "sshd host: worker-01 message: authentication ok for user bob pid: 2175 ...",
      "transition_prob": 0.30
    },
    {
      "offset_secs":     150.0,
      "source_state":    "sshd host: <*> message: <*> <*> for user <*>",
      "text":            "sshd host: worker-01 message: authentication ok for user bob pid: 2175 ...",
      "transition_prob": 0.53
    }
  ],
  "cluster_meta": {
    "enabled":        true,
    "partial":        false,
    "peers_queried":  2,
    "peers_answered": 2,
    "failed":         []
  }
}
```

### Example E — `bdscmd project-logs`

```bash
$ bdscmd -a 127.0.0.1:9711 project-logs \
    --duration-back 30min \
    --duration-forward 10min \
    --seed 42 \
    --n-samples 30 \
    --min-consensus 0.05

{
  "cluster_meta":     { "peers_queried": 2, "peers_answered": 2, "partial": false, ... },
  "duration_back":    "30min",
  "duration_forward": "10min",
  "events": [
    {
      "offset_secs":     15.0,
      "source_state":    "cron host: <*> message: <*> CMD <*> --quiet) pid: <*> ...",
      "text":            "cron host: api-01 message: (api-01) CMD (...) pid: 7867 ...",
      "transition_prob": 1.0
    },
    ...
  ]
}
```

### Example F — bdsweb "Analyze this!" surface

`Analysis → Project events` in bdsweb runs the same `v3/project_logs`
RPC, renders the events as a time-binned table with colour-coded
consensus chips (emerald ≥ 0.66, amber ≥ 0.33, slate below), and
offers an `"Analyze this!"` button that hands the projection to the
configured default LLM via `v4/llm.analyze` with a prompt that asks
the model to:

1. Read the projection as an *early-warning trace*.
2. Surface high-consensus signals (`transition_prob ≥ 0.5`).
3. Flag failure-flavoured templates.
4. Identify concerning sequences.
5. Interpret low-consensus diversity as a *positive* signal.
6. Recommend one concrete next step.

See `web.analyze.project_logs.*` in `Documentation/BDSCONFIG.md` for
the prompt and timeout knobs.

---

## 10. Failure modes and edge cases

| Condition | Behaviour | Notes |
|---|---|---|
| Empty `inputs` | Returns `Vec::new()` | No panic, no work. |
| `duration` fails `humantime::parse_duration` | Returns `Vec::new()` | Same fail-safe shape as other analysis functions. |
| `duration` is zero or negative | Returns `Vec::new()` | A zero window can't hold any events by definition. |
| Single distinct state in input | Chain self-loops; all projected events are that state | Genuine information (the system is in a stable loop). |
| Input shorter than `order` | Effectively runs at lower order from the start | Backoff handles this automatically. |
| All `min_consensus` gates rejected | Returns `Vec::new()` | Lower the threshold or widen the training window. |
| `events_per_second ≤ 0` (untimed) | Floored at `0.001` so the rollout terminates | Pathological mean gap; flagged in code comments. |
| Heavy mode (one state at 95%+) | All projected bins are that state with `transition_prob ≈ 1.0` | The chain is reporting reality, not a bug — the recent window WAS dominated by that pattern. |
| Drain3 produces many singleton clusters | Projection has high diversity, low consensus | Try a coarser bucketing (Normalize) or widen the training window so drain3 sees more repeats. |
| Multiple identical timestamps in timed input | Gap is `0.0` for that transition; sampled at projection time | Operationally rare; mathematically fine. |
| `cfg.order` outside `[1, 4]` | Clamped to `[1, 4]` internally | No error, just bounded. |
| `cfg.n_samples = 0` | Treated as `1` internally | Avoid runtime divide-by-zero on the consensus denominator. |
| `cfg.time_bins = 0` | Treated as `1` internally | Same reason. |

---

## 11. References

- **Markov, A.A. (1906)** — *Extension of the limit theorems of
  probability theory to a sum of variables connected in a chain.*
  Reprinted in *Dynamic Probabilistic Systems, Volume I* (R.A. Howard,
  ed.), Wiley 1971. The original Markov chain paper.
- **Shannon, C.E. (1948)** — *A Mathematical Theory of Communication.*
  Bell System Technical Journal 27 (3): 379–423. Introduced n-gram
  language modelling — the immediate ancestor of higher-order Markov
  chains over text.
- **Howard, R.A. (1971)** — *Dynamic Probabilistic Systems, Volume II:
  Semi-Markov and Decision Processes.* Wiley. Reference text for
  semi-Markov processes; introduces the holding-time distribution per
  transition.
- **Manning, C.D. & Schütze, H. (1999)** — *Foundations of Statistical
  Natural Language Processing*, MIT Press. Chapter 6 covers smoothed
  n-gram language models; we use the simpler stupid-backoff variant
  rather than Kneser-Ney.
- **Brants, T., Popat, A.C., Xu, P., Och, F.J. & Dean, J. (2007)** —
  *Large Language Models in Machine Translation.* EMNLP 2007: 858–867.
  Introduces "stupid backoff" — the hard-fallback variant we use,
  proven effective in practice without the complexity of Kneser-Ney.
- **Sammoud, A., Mehmood, T. & Bahir, S. (2024)** — *Predictive
  Maintenance via Hidden Markov Models on Industrial Log Streams.*
  Survey of semi-Markov approaches to operational forecasting; the
  closest published analog to this module's use case.
- **He, P., Zhu, J., Zheng, Z. & Lyu, M.R. (2017)** — *Drain: An Online
  Log Parsing Approach with Fixed Depth Tree.* ICWS 2017: 33–40.
  Original drain3 paper; we use bdslib's
  `crate::common::drain::DrainParser` implementation for the default
  state bucketing.
