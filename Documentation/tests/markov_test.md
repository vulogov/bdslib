# markov tests

**Module:** `bdslib::analysis::markov` — semi-Markov log/event projection

**Test location:** `src/analysis/markov.rs` (in-module `#[cfg(test)] mod tests`)

Verifies the contract of `markov_project` / `markov_project_with` and
the timed siblings `markov_project_timed` / `markov_project_timed_with`:
result shape, every config knob, the three bucketing modes,
seed-determined determinism, and end-to-end behaviour over operationally
realistic input shapes.

Algorithm reference: [`Documentation/Algorithm/MARKOV.md`](../Algorithm/MARKOV.md).

## Test functions

### Shape / edge cases

| Test | What it verifies |
|---|---|
| `empty_input_returns_empty` | `markov_project(&[], "1h")` returns `Vec::new()` — no panic, no work. |
| `bad_duration_returns_empty` | An unparseable humantime string (`"garbage"`) returns an empty vector instead of erroring. |
| `zero_duration_returns_empty` | `"0s"` returns empty — a zero window can't hold any events. |
| `respects_duration_window` | Every emitted `offset_secs` is `≤ duration_secs`. |

### Determinism

| Test | What it verifies |
|---|---|
| `seed_determinism` | Two calls with the same `(inputs, duration, cfg.seed)` return byte-identical `Vec<ProjectedEvent>` (same length, same `source_state`, same `offset_secs`, same `transition_prob`).  This is the test that catches non-deterministic `HashMap` iteration in `sample_next`, `sample_gap`, and `aggregate`. |
| `projected_events_are_time_ordered` | Output is sorted by `offset_secs` ascending — invariant that downstream consumers (the bdsweb event table, the LLM analyze prompt) rely on. |

### Bucketing modes

| Test | What it verifies |
|---|---|
| `drain3_groups_near_duplicates` | 30 lines of `"kernel: NMI received on CPU {i}"` for `i ∈ 1..=30` produce ONE drain3 state (the template `kernel: NMI received on CPU <*>`).  Every projected event carries the same `source_state`. |
| `normalize_collapses_digits` | `Bucketing::Normalize` collapses `"req 1 ok"`, `"req 2 ok"`, `"req 9001 ok"` into one state whose label carries the `<N>` placeholder. |
| `identity_treats_each_string_as_own_state` | With `Bucketing::Identity`, an alternating `"alpha" / "beta"` input produces projected events drawn only from those two states — the chain learns `P(beta\|alpha) = P(alpha\|beta) = 1`. |

### Core algorithm

| Test | What it verifies |
|---|---|
| `single_state_input_stays_in_state` | A `["only", "only", …]` corpus produces projections that stay in the `only` state — the chain self-loops correctly. |
| `timed_variant_uses_empirical_gaps` | An input with consistent 10-second gaps produces projections with ~10-second gaps even when `cfg.events_per_second = 100.0` would imply 10-ms gaps if the fallback path were taken.  Confirms that empirical gap learning takes precedence. |

### Config knobs

| Test | What it verifies |
|---|---|
| `min_consensus_filters_low_support` | A strict `min_consensus = 0.95` produces a subset of (or empty) results compared to `min_consensus = 0.0`.  Monotonicity check. |
| `max_events_caps_output` | `cfg.max_events = 3` caps the returned `Vec` at exactly 3 events. |

## Determinism construction

The non-obvious determinism requirements satisfied by the in-module sort
calls:

| Site | What's sorted | Why |
|---|---|---|
| `sample_next` | per-context distribution by `state_id` ascending | `HashMap` iteration is non-deterministic across separately-built maps even with identical contents (per-instance random hasher).  Without this sort the seeded RNG draws against an unpredictable enumeration order. |
| `sample_gap` "into-next" fallback | flattened gaps by f64 value ascending | Same `HashMap`-iteration reason; here the sort key is the gap value itself, since the random index translates into a specific value. |
| `sample_gap` "global" fallback | same | Same reason. |
| `aggregate` per-bin votes | by vote-count descending, tie-break by `state_id` ascending | Two states earning identical votes need a deterministic emission order — otherwise the first emitted event in the bin flips between runs. |

The `seed_determinism` test exercises all three sort sites in the same
call (via the standard projection path); a regression in any one would
fail it.

## Coverage gaps (intentionally not in the unit tests)

- **Cross-process determinism.** Same `(inputs, duration, seed)` on two
  different machines should produce the same projection.  The test
  suite uses `rand::rngs::StdRng` which is deterministic across
  platforms (ChaCha8-backed), and the sort-based determinism fixes
  remove the `HashMap` randomness.  Property is asserted by inspection
  rather than tested directly — adding it would require a fixed
  expected-output corpus.
- **Drain3 cluster-id stability.** The default `Drain3` bucketing
  depends on `crate::common::drain::DrainParser`, whose cluster IDs
  are assigned in arrival order; identical input always produces
  identical IDs within a single run.  We don't pin a snapshot of
  cluster IDs in this test suite — `drain3_groups_near_duplicates`
  asserts the structural property (one cluster) instead.
- **Cross-version backwards compatibility.** If we change the
  `MarkovProjectionConfig` defaults or the smoothing strategy,
  seed-determined projections will change.  This is intentional — the
  `seed` knob is for reproducibility within a config + version, not
  across upgrades.

## See also

- [`Documentation/Algorithm/MARKOV.md`](../Algorithm/MARKOV.md) — full
  algorithm reference, complexity, determinism guarantees, references.
- `tests/` directory at the workspace root — integration tests for
  shard-aware wrappers and the JSON-RPC layer that wraps this module.
