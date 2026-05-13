# Synthetic data generator — `generate_realistic_data`

bdsnode can run a background tokio task that periodically injects
**fake** telemetry + log records into the local ingest pipeline.
The records come from
[`bdslib::common::realistic::generate`](../src/common/realistic.rs)
— three layers (background noise, incident cascades, anomalies)
designed to exercise the full search / RCA / k-NN / template-mining
pipeline so demos look realistic.

> **Production deployments MUST NOT enable this.**  Every status
> surface (`v2/status`, the bdsweb dashboard, the bdsnode startup
> log) emits an unmissable "SYNTHETIC DATA" warning when the
> generator is armed, but the contract is on the operator to keep
> it off in prod.

---

## Enabling

Two ways, either works:

### bds.hjson

```hjson
generate_realistic_data: {
  enabled:        true        // master switch (default false)
  interval_secs:  60          // seconds between batches, clamped [5, 86400]
  duration:       "6h"        // humantime window each batch covers
  total:          2000        // target records per batch
  scenarios:      3           // incident cascades per batch
  noise_ratio:    0.7         // 0.0–0.95, background-noise fraction
  anomaly_ratio:  0.02        // 0.0–0.10, rare-record fraction
  seed:           null        // null = entropy, integer = deterministic
}
```

All keys are optional; defaults shown above apply when omitted.

### CLI flag

```bash
bdsnode --config bds.hjson --generate_realistic_data
```

The flag forces `enabled=true` **regardless** of the hjson value.
Useful when you want demo mode for a single run without editing
the config file.  All other knobs (interval, ratios, scenarios)
still come from the hjson block; defaults apply when the block
is absent.

---

## What happens at startup

When generation is armed, bdsnode prints a multi-line WARN banner
right after the runtime initialises:

```
┌──────────────────────────────────────────────────────────────────────┐
│                                                                      │
│   ⚠  SYNTHETIC DATA GENERATION IS ENABLED  ⚠                         │
│                                                                      │
│   This node is injecting artificially-generated telemetry, logs,     │
│   and incident scenarios into the ingest pipeline on a timer.        │
│   Anything you observe through search / analysis / dashboards is     │
│   NOT REAL OPERATIONAL DATA.                                         │
│                                                                      │
│   Disable for production by removing `generate_realistic_data` from  │
│   bds.hjson and dropping the --generate_realistic_data CLI flag.     │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
[dev_data] generator armed — interval=60s duration="6h" total=2000 …
[dev_data] enqueued batch — n=2068 took=4ms
```

The first batch fires **immediately** so dashboards have data to
render within seconds of process start.  Subsequent batches fire
every `interval_secs`.

---

## How it works

```
                 every interval_secs
                          ↓
         ┌─────────────────────────────────┐
         │  bdslib::common::realistic ::    │
         │  generate(RealisticConfig)       │
         │  → Vec<serde_json::Value>        │
         └────────────────┬─────────────────┘
                          ↓
         bdslib::pipe::send_many("ingest", docs)
                          ↓
              (standard ingest worker pool)
                          ↓
           shard storage + FTS + vector + drain
```

Records go through **the same `"ingest"` pipe** that
`v2/add.batch` uses — so they exercise the full ingest path
(deduplication, sharding, full-text indexing, vector embedding,
template mining), not a fast path.  This makes demo runs
representative of production ingest behaviour at the cost of one
extra batch per `interval_secs`.

`pipe::send_many` is bounded by `ingest_channel_capacity`; under
sustained pressure the generator's batch may fail to enqueue
(`channel "ingest" is full`).  That failure increments
`v2/status.dev_data.errors_lifetime` and logs a WARN — the
generator does NOT block waiting for room.

---

## v2/status surface

`v2/status` always carries a `dev_data` block:

```json
"dev_data": {
  "enabled":            true,
  "records_lifetime":   12480,
  "records_last_batch": 2068,
  "batches_emitted":    6,
  "last_run_ts":        1715712100,
  "last_run_ms":        4,
  "errors_lifetime":    0,

  /* present only when server::dev_data::start has been called: */
  "config_enabled":     true,
  "interval_secs":      60,
  "duration":           "6h",
  "total_per_batch":    2000,
  "scenarios":          3,
  "noise_ratio":        0.7,
  "anomaly_ratio":      0.02,
  "seed":               null
}
```

`enabled` is the field the bdsweb dashboard checks to render its
red banner.  It flips to `true` once at startup and stays true for
the lifetime of the process — no live "disable" toggle exists,
because that would let an operator silently mask the banner mid-run.

---

## bdsweb dashboard

When `v2/status.dev_data.enabled == true` the dashboard
(`/`) prepends a full-width red banner above the regular content:

> **⚠ SYNTHETIC DATA — THIS NODE IS RUNNING IN DEMO MODE**
> Records visible on this dashboard are produced by
> `bdslib::common::realistic::generate` every Ns
> (M target records per batch; X batches emitted, Y records total).
> **Nothing here is real operational telemetry.**

The banner shows the live counters refreshed at the dashboard's
configured `dashboard_refresh_secs` cadence.

---

## Tuning

| Knob            | Recommended values                                                                |
|-----------------|-----------------------------------------------------------------------------------|
| `interval_secs` | `60` for steady demos; `10` for rapid load-test of the ingest path.              |
| `total`         | `200`–`2000`.  Each batch is bursty — `pipe::send_many` enqueues all docs in one call. |
| `scenarios`     | `2`–`5`.  Each cascade emits 30–60 records and gives RCA something to find.       |
| `noise_ratio`   | `0.7` (default).  Higher → more "boring" baseline traffic that denoise should strip. |
| `anomaly_ratio` | `0.02` (default).  Higher → more surprises for the n-gram anomaly detector.      |
| `seed`          | Pin to an integer for reproducible test datasets (CI, regression).               |

For RCA / k-NN demos: low interval (e.g. `30`) + `scenarios: 5`
gives a steady supply of cascades.  For trend / aggregate demos:
high interval (e.g. `300`) + larger `total` gives smoother long-term
graphs.

---

## Cluster behaviour

Each node runs its own generator independently.  The records are
pushed through the **local** ingest pipe, NOT through `v3/add.batch`
— so they do **not** replicate to peers automatically.  Two
deployment patterns:

1. **Single-node demo** — run on one bdsnode, point bdsweb at it.
   Simple, single source of truth.
2. **Multi-node demo** — enable on every peer.  Each node ingests
   its own disjoint stream; cluster reads fan out across all of
   them, returning ~3x the volume.  Realistic, but the data on
   each peer isn't a replica.

If you want the records to land replicated, you'd need to wrap
the generator to call a cluster-aware ingest path (`v3/add.batch`)
instead of the local pipe — not currently implemented.

---

## Risks

1. **Anyone with dashboard access sees the banner.**  It's loud
   on purpose.  If you have a public-facing bdsweb instance, do
   NOT enable the generator there.
2. **The pipe is bounded.**  Under sustained pressure (slow shard
   storage, undersized `n_workers`), the generator's batches
   may fail to enqueue.  Watch `errors_lifetime` in
   `v2/status.dev_data`.
3. **The generator is not RAG-savvy.**  The fake records look like
   ops telemetry, but they don't form a coherent narrative that
   LLM RAG (`v3/help`, `v4/llm.analyze`) can summarise meaningfully.
4. **No live disable.**  Stop and restart bdsnode to turn the
   generator off.  Mid-flight kill = the in-flight batch may be
   half-ingested, but no inconsistency — each record is independent.

---

## Quick reference

| What                     | Where                                                 |
|--------------------------|-------------------------------------------------------|
| Config                   | `generate_realistic_data:` block in `bds.hjson`       |
| CLI override             | `bdsnode --generate_realistic_data`                   |
| Generator entry point    | `bdslib::common::realistic::generate`                  |
| Stats                    | `bdslib::dev_data::stats` (atomic counters, `OnceLock`)|
| Tokio task               | `src/bin/bdsnode/server/dev_data.rs`                  |
| Status RPC field         | `v2/status.dev_data`                                  |
| Dashboard banner         | bdsweb `/` (red, dismissable only by disabling)       |
| Default state            | **disabled** (operators opt in explicitly)            |
