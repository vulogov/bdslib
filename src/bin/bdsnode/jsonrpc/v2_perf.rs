//! `v2/perf` — snapshot of every in-process latency series tracked by
//! [`bdslib::perf`].
//!
//! Returns one entry per named series with `min_us`, `max_us`,
//! `mean_us`, `p50_us`, `p95_us`, `p99_us`, `n_total` (lifetime samples)
//! and `n_recent` (samples in the 1024-slot ring buffer).
//!
//! Series naming convention (defined in `bdslib::perf` module docs):
//!
//! - `ingest.flush`           — DuckDB batch-insert duration
//! - `ingest.lag`             — wall-clock from first doc in batch to flush
//! - `ingest.batch_size`      — flushed batch sizes (samples are record counts)
//! - `fanout.peer.<node_id>`  — v3/* read fan-out RTT, per peer
//! - `fanout.method.<m>`      — v3/* read fan-out RTT, per RPC method
//! - `replicate.peer.<id>`    — v3/* write replication RTT, per peer
//! - `replicate.method.<m>`   — v3/* write replication RTT, per RPC method
//!
//! Operators consume this via the dashboard "Performance" tile, the
//! `bdscmd perf` subcommand, or Prometheus scraping (left as a future
//! exporter — the registry is open).

use jsonrpsee::RpcModule;

#[derive(serde::Deserialize)]
struct SlowParams {
    /// Optional name-prefix filter (`"fanout."`, `"ingest."`, …).
    #[serde(default)]
    name_prefix: String,
    /// Optional age cap in seconds.  `0` (default) returns everything.
    #[serde(default)]
    since_secs:  u64,
}

pub fn register(module: &mut RpcModule<()>) {
    module
        .register_async_method("v2/perf", |_params, _ctx, _| async move {
            let series = bdslib::perf::registry().snapshot_all();
            let mut out = serde_json::Map::with_capacity(series.len());
            for (name, s) in series {
                out.insert(name, serde_json::json!({
                    "n_total":  s.n_total,
                    "n_recent": s.n_recent,
                    "min_us":   s.min_us,
                    "max_us":   s.max_us,
                    "mean_us":  s.mean_us,
                    "p50_us":   s.p50_us,
                    "p95_us":   s.p95_us,
                    "p99_us":   s.p99_us,
                }));
            }
            Ok::<serde_json::Value, jsonrpsee::types::ErrorObject>(
                serde_json::Value::Object(out)
            )
        })
        .unwrap();

    // ── slow-query log ────────────────────────────────────────────────
    //
    // Returns the in-process slow-query ring (newest first), capped at
    // `bdslib::perf::SLOW_LOG_CAPACITY` entries.  Optional `name_prefix`
    // filter (substring match would mix unrelated series; prefix is the
    // intuitive form for "all fanout.*" or "all ingest.*").  Optional
    // `since_secs` window keeps responses small on busy nodes.
    module
        .register_async_method("v2/perf.slow_queries", |params, _ctx, _| async move {
            let p: SlowParams = params.parse().unwrap_or(SlowParams {
                name_prefix: String::new(),
                since_secs:  0,
            });
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let cutoff = if p.since_secs == 0 { 0 } else { now.saturating_sub(p.since_secs) };

            let entries: Vec<serde_json::Value> = bdslib::perf::slow_snapshot()
                .into_iter()
                .filter(|e| p.name_prefix.is_empty() || e.name.starts_with(&p.name_prefix))
                .filter(|e| e.ts >= cutoff)
                .map(|e| serde_json::json!({
                    "name":       e.name,
                    "elapsed_us": e.elapsed_us,
                    "elapsed_ms": e.elapsed_us / 1000,
                    "ts":         e.ts,
                }))
                .collect();

            Ok::<serde_json::Value, jsonrpsee::types::ErrorObject>(serde_json::json!({
                "threshold_us": bdslib::perf::slow_threshold_us(),
                "threshold_ms": bdslib::perf::slow_threshold_us() / 1000,
                "entries":      entries,
            }))
        })
        .unwrap();
}
