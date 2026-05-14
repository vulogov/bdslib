# v2/perf

Snapshot of every in-process latency series tracked by the
`bdslib::perf` registry on this node.

The registry is populated by hot-path instrumentation: the ingest
flusher (`ingest.flush`, `ingest.lag`, `ingest.batch_size`), the
cluster read fan-out (`fanout.peer.<node_id>`, `fanout.method.<m>`),
and the cluster write replication path (`replicate.peer.<node_id>`,
`replicate.method.<m>`).  Sampling is lock-free and bounded to a
1024-slot ring per series, so calling `v2/perf` is essentially free.

## Parameters

This method accepts no parameters.

## Response

```json
{
  "ingest.flush": {
    "n_total":  4821,
    "n_recent": 1024,
    "min_us":   8123,
    "max_us":   142008,
    "mean_us":  27840,
    "p50_us":   24512,
    "p95_us":   68240,
    "p99_us":   118400
  },
  "ingest.lag": {
    "n_total":  4821,
    "n_recent": 1024,
    "min_us":   12,
    "max_us":   503010,
    "mean_us":  142031,
    "p50_us":   89102,
    "p95_us":   480123,
    "p99_us":   501982
  },
  "ingest.batch_size": {
    "n_total":  4821,
    "n_recent": 1024,
    "min_us":   1,
    "max_us":   500,
    "mean_us":  486,
    "p50_us":   500,
    "p95_us":   500,
    "p99_us":   500
  },
  "fanout.peer.0193…0a": {
    "n_total":  281,
    "n_recent": 281,
    "min_us":   1812,
    "max_us":   84021,
    "mean_us":  9120,
    "p50_us":   8400,
    "p95_us":   42010,
    "p99_us":   78230
  },
  "fanout.method.v2/search": { … },
  "replicate.peer.0193…0a":  { … },
  "replicate.method.v2/add": { … }
}
```

| Field      | Unit   | Description |
|------------|--------|-------------|
| `n_total`  | count  | Lifetime samples recorded for this series since process start. May exceed `n_recent` — the ring buffer wraps. |
| `n_recent` | count  | Samples currently held in the ring buffer (≤ 1024). p50/p95/p99 are computed from this window. |
| `min_us`   | µs     | Lifetime minimum sample seen. Atomic; survives ring-buffer wrap. |
| `max_us`   | µs     | Lifetime maximum sample seen. Atomic; survives ring-buffer wrap. |
| `mean_us`  | µs     | Mean of the ring-buffer window. |
| `p50_us`   | µs     | Median of the ring-buffer window. |
| `p95_us`   | µs     | 95th-percentile of the ring-buffer window. |
| `p99_us`   | µs     | 99th-percentile of the ring-buffer window. |

## Series conventions

Standard prefixes — instrumentation in other modules can register
arbitrary labels, so this list is not exhaustive:

| Prefix              | Source                                        |
|---------------------|-----------------------------------------------|
| `ingest.flush`      | `server::add::flush()` — DuckDB batch insert  |
| `ingest.lag`        | wall-clock from first doc in batch to flush   |
| `ingest.batch_size` | flushed batch sizes (samples are record counts, not µs) |
| `fanout.peer.<id>`  | `cluster::fanout::fan_out_v2` per-peer RTT    |
| `fanout.method.<m>` | `fan_out_v2` per RPC method (aggregated across peers) |
| `replicate.peer.<id>` | `cluster::replication::replicate_to_all` per-peer RTT |
| `replicate.method.<m>` | replication per RPC method |
| `embed.hit`         | query-embedding cache hits (cheap, ~µs)       |
| `embed.miss`        | query-embedding cache misses (real ONNX inference) |
| `shard.fts_scored`  | per-shard FTS lookup, IDs+score only          |
| `shard.fts_with_ts` | per-shard FTS lookup with timestamp join      |
| `shard.vector_precomputed` | per-shard HNSW + MMR rerank, full bodies fetched |
| `shard.vector_scored_precomputed` | per-shard HNSW + MMR rerank, IDs+ts+score only |

`ingest.batch_size` is the only series whose samples are not µs —
they're record counts.  The percentiles still apply: `p50=500`
means half of flushes carry the full `pipe_batch_size`.

## Headline numbers on `v2/status`

`v2/status.perf` carries a compact subset of these values so a
dashboard tile can render without a second RPC:

```json
"perf": {
  "ingest_flush_p50_us":  24512,
  "ingest_flush_p95_us":  68240,
  "ingest_flush_p99_us":  118400,
  "ingest_flush_n_total": 4821,
  "ingest_lag_p50_us":    89102,
  "ingest_lag_p95_us":    480123,
  "fanout_p95_us_max":    42010,
  "replicate_p95_us_max": 38120
}
```

`fanout_p95_us_max` and `replicate_p95_us_max` are the maximum
p95 across every `fanout.method.*` / `replicate.method.*` series
respectively — one number per side that surfaces the slowest RPC
in this window.

## bdscmd

```
$ bdscmd perf
{ "ingest.flush": { ... }, ... }

$ bdscmd perf --name fanout.peer.
# filter to per-peer fan-out series only

$ bdscmd perf --name ingest
# filter to the ingest pipeline series
```

## Slow-query log

A companion method `v2/perf.slow_queries` exposes a bounded ring of
recent outliers — calls that exceeded
`perf.slow_query_threshold_ms` (default 500 ms).  Every
`perf::time` call participates automatically — no per-handler
instrumentation needed.

```json
{
  "threshold_us": 500000,
  "threshold_ms": 500,
  "entries": [
    { "name": "ingest.flush",            "elapsed_us": 1284122, "elapsed_ms": 1284, "ts": 1747186020 },
    { "name": "fanout.method.v2/search", "elapsed_us":  812401, "elapsed_ms":  812, "ts": 1747186015 }
  ]
}
```

Entries are newest-first, ring capped at 100 events.  Optional
`name_prefix` and `since_secs` parameters keep responses small:

```
$ bdscmd perf-slow                              # everything
$ bdscmd perf-slow --name-prefix fanout.        # only fan-out outliers
$ bdscmd perf-slow --name-prefix ingest. --since-secs 600
$ bdscmd perf-slow --since-secs 3600            # last hour
```

Use the slow log to spot rare outliers — a single 2-second call
among 1000 fast ones can be hidden by p95 sample dilution but will
show up here immediately.  When the log is empty, the node is
healthy with respect to the configured threshold.

Set `perf.slow_query_threshold_ms: 0` in `bds.hjson` to disable
slow-log capture entirely (the threshold-zero shortcut bypasses
the ring write on every `perf::time` call).

## Errors

`v2/perf` cannot fail under normal operation — the registry is
process-local and lazily initialised.  Empty series (never touched
since startup) are simply absent from the response.
