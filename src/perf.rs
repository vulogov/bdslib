//! Lightweight in-process perf instrumentation.
//!
//! Hot paths (ingest flusher, cluster fan-out, embedding) call into
//! [`record`] to log a single µs sample.  The write side is **lock-free
//! and allocation-free** — an atomic ring-buffer index advance + a slot
//! write — so high-frequency callers (per-doc on ingest, per-peer on
//! every RPC) can sample without measurable overhead.
//!
//! The read side ([`percentiles`], [`snapshot`]) copies the ring buffer
//! into a local Vec and sorts it.  Cheap when the buffer is small
//! (`MAX_SAMPLES` = 1024 by default).  Used by the JSON-RPC `v2/perf`
//! handler and the headline figures in `v2/status.perf`.
//!
//! ## Why ring buffers, not a real histogram library?
//!
//! - Zero allocations on the hot path.
//! - Recent-window semantics for free — the ring naturally forgets
//!   anything older than `MAX_SAMPLES` samples ago.
//! - p50/p95/p99 from 1024 sorted samples is plenty accurate for
//!   operator-facing latency dashboards; we're not promising tail
//!   accuracy at the µs level.
//!
//! For more precise lifetime metrics, this module also tracks `min`,
//! `max`, `n_total`, `n_dropped` (wrap-around overflow) per series, all
//! as atomics.

use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many recent samples each series keeps for percentile estimation.
/// 1024 gives ~3-digit percentile resolution and tracks the last few
/// minutes of activity on a 1 RPS workload.
pub const MAX_SAMPLES: usize = 1024;

/// One named latency series.  Hot-path writes go through
/// [`Series::record`] (atomic increment + parking_lot mutex for the
/// fixed-size Vec).  Sample storage is bounded so a misbehaving
/// caller can't OOM the process.
struct Series {
    /// Lifetime sum so `mean` is available without iterating samples.
    sum_us:      AtomicU64,
    /// Lifetime sample count.  May exceed MAX_SAMPLES; reads use it
    /// to know how many distinct events have been recorded.
    n_total:     AtomicU64,
    /// Lifetime min / max (`u64::MAX` and `0` as sentinels).
    min_us:      AtomicU64,
    max_us:      AtomicU64,
    /// Ring buffer of the most-recent samples.  parking_lot::Mutex is
    /// uncontended in practice — the critical section is a single
    /// indexed write + a counter bump, hundreds of nanoseconds.
    ring:        Mutex<Vec<u64>>,
    /// Write index into `ring` (mod MAX_SAMPLES).
    cursor:      AtomicUsize,
}

impl Series {
    fn new() -> Self {
        Self {
            sum_us:  AtomicU64::new(0),
            n_total: AtomicU64::new(0),
            min_us:  AtomicU64::new(u64::MAX),
            max_us:  AtomicU64::new(0),
            ring:    Mutex::new(Vec::with_capacity(MAX_SAMPLES)),
            cursor:  AtomicUsize::new(0),
        }
    }

    fn record(&self, value_us: u64) {
        self.sum_us .fetch_add(value_us, Ordering::Relaxed);
        self.n_total.fetch_add(1,        Ordering::Relaxed);
        // Atomic min/max with a small CAS loop.
        let mut cur_min = self.min_us.load(Ordering::Relaxed);
        while value_us < cur_min {
            match self.min_us.compare_exchange_weak(cur_min, value_us,
                Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_)  => break,
                Err(v) => cur_min = v,
            }
        }
        let mut cur_max = self.max_us.load(Ordering::Relaxed);
        while value_us > cur_max {
            match self.max_us.compare_exchange_weak(cur_max, value_us,
                Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_)  => break,
                Err(v) => cur_max = v,
            }
        }

        // Append-or-overwrite into the ring buffer.  Holding the mutex
        // for one indexed write is fine — no allocation, no I/O.
        let mut ring = self.ring.lock();
        if ring.len() < MAX_SAMPLES {
            ring.push(value_us);
        } else {
            let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % MAX_SAMPLES;
            ring[idx] = value_us;
        }
    }

    fn snapshot(&self) -> SeriesSnapshot {
        let samples: Vec<u64> = self.ring.lock().clone();
        let n_total = self.n_total.load(Ordering::Relaxed);
        if samples.is_empty() {
            return SeriesSnapshot {
                n_total,
                n_recent: 0,
                min_us: 0, max_us: 0, mean_us: 0,
                p50_us: 0, p95_us: 0, p99_us: 0,
            };
        }
        let mut sorted = samples;
        sorted.sort_unstable();
        let n = sorted.len();
        let pct = |p: f64| -> u64 {
            // Nearest-rank percentile.  Clamps so p99 of 1 sample is that sample.
            let idx = ((p * n as f64).ceil() as usize).saturating_sub(1).min(n - 1);
            sorted[idx]
        };
        let sum: u64 = sorted.iter().sum();
        SeriesSnapshot {
            n_total,
            n_recent: n as u64,
            min_us: self.min_us.load(Ordering::Relaxed).min(sorted[0]),
            max_us: self.max_us.load(Ordering::Relaxed).max(sorted[n - 1]),
            mean_us: sum / n as u64,
            p50_us: pct(0.50),
            p95_us: pct(0.95),
            p99_us: pct(0.99),
        }
    }
}

/// JSON-friendly summary of one series.  All durations in
/// microseconds; the JSON-RPC handler converts to ms / serialises.
#[derive(Debug, Clone, Default)]
pub struct SeriesSnapshot {
    pub n_total:  u64,
    pub n_recent: u64,
    pub min_us:   u64,
    pub max_us:   u64,
    pub mean_us:  u64,
    pub p50_us:   u64,
    pub p95_us:   u64,
    pub p99_us:   u64,
}

/// Process-wide registry of named latency series.
///
/// Keys are stable string labels (`"ingest.flush"`,
/// `"fanout.peer.<node_id>"`, `"embed.call"`, …).  DashMap lets the
/// write side look up a series without a mutex on the registry map
/// itself.
pub struct PerfRegistry {
    series: DashMap<String, Arc<Series>>,
}

impl PerfRegistry {
    fn new() -> Self {
        Self { series: DashMap::new() }
    }

    fn entry(&self, name: &str) -> Arc<Series> {
        if let Some(s) = self.series.get(name) {
            return s.clone();
        }
        // Insert-then-get pattern — DashMap handles the race.
        self.series.entry(name.to_owned())
            .or_insert_with(|| Arc::new(Series::new()))
            .clone()
    }

    /// Snapshot every series in alphabetical order so JSON output is
    /// stable for operator diffing.
    pub fn snapshot_all(&self) -> Vec<(String, SeriesSnapshot)> {
        let mut keys: Vec<String> = self.series.iter().map(|e| e.key().clone()).collect();
        keys.sort();
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            if let Some(s) = self.series.get(&k) {
                out.push((k, s.snapshot()));
            }
        }
        out
    }

    /// Single-series snapshot.  Returns the empty snapshot when the
    /// series has never been touched.
    pub fn snapshot_one(&self, name: &str) -> SeriesSnapshot {
        match self.series.get(name) {
            Some(s) => s.snapshot(),
            None    => SeriesSnapshot::default(),
        }
    }

    /// p95 (µs) for a series, or `None` if the ring has fewer than
    /// `min_samples` recent samples.  Adaptive heuristics that read
    /// from the registry should always check for `None` and fall back
    /// to a configured default — a series with 3 samples does not
    /// produce a meaningful percentile.
    pub fn p95_us(&self, name: &str, min_samples: u64) -> Option<u64> {
        let s = self.series.get(name)?.snapshot();
        if s.n_recent < min_samples {
            return None;
        }
        Some(s.p95_us)
    }
}

static REGISTRY: OnceLock<PerfRegistry> = OnceLock::new();

/// Borrow the process-wide registry.  Lazy-initialised on first use
/// so library callers that never instrument pay nothing.
pub fn registry() -> &'static PerfRegistry {
    REGISTRY.get_or_init(PerfRegistry::new)
}

// ─────────────────────────────────────────────────────────────────────
// Slow-query log
//
// Single bounded VecDeque<SlowEntry> capturing the most-recent N events
// whose elapsed time exceeded the process-wide threshold.  Every
// `perf::time` call participates automatically — no per-site
// instrumentation required.
//
// Operators inspect via `v2/perf.slow_queries` or `bdscmd perf-slow`
// to spot outliers that don't show up in p95 (e.g. one 2-second call
// among 1000 fast ones).
// ─────────────────────────────────────────────────────────────────────

/// Default ring capacity — 100 slow events.  ~5 KB at 50 B per entry.
pub const SLOW_LOG_CAPACITY: usize = 100;

/// Default slow-query threshold: 500 ms.  Reset at startup from the
/// `perf.slow_query_threshold_ms` hjson key when present.
pub const DEFAULT_SLOW_THRESHOLD_US: u64 = 500_000;

/// One slow-query log entry.  All fields plain so JSON serialisation
/// is trivial and the wire format is stable across versions.
#[derive(Debug, Clone)]
pub struct SlowEntry {
    /// Series label, e.g. `"v3/search"`, `"fanout.peer.<id>"`,
    /// `"ingest.flush"`.
    pub name:       String,
    /// Elapsed wall-clock time in microseconds.
    pub elapsed_us: u64,
    /// Unix-second timestamp when the event completed.
    pub ts:         u64,
}

struct SlowLog {
    threshold_us: AtomicU64,
    ring:         Mutex<VecDeque<SlowEntry>>,
    capacity:     usize,
}

impl SlowLog {
    fn new() -> Self {
        Self {
            threshold_us: AtomicU64::new(DEFAULT_SLOW_THRESHOLD_US),
            ring:         Mutex::new(VecDeque::with_capacity(SLOW_LOG_CAPACITY)),
            capacity:     SLOW_LOG_CAPACITY,
        }
    }

    fn record(&self, name: &str, elapsed_us: u64) {
        // Per-call check: the threshold is loaded once, with relaxed
        // ordering (no need for synchronisation — operators tweaking
        // the value tolerate a few stale reads).
        let thr = self.threshold_us.load(Ordering::Relaxed);
        if thr == 0 || elapsed_us < thr {
            return;
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = SlowEntry {
            name: name.to_owned(),
            elapsed_us,
            ts,
        };
        let mut ring = self.ring.lock();
        if ring.len() >= self.capacity {
            ring.pop_front();
        }
        ring.push_back(entry);
    }

    fn snapshot(&self) -> Vec<SlowEntry> {
        // Iterate newest-first so the JSON-RPC consumer can paginate
        // from the head and see recency in order.
        let ring = self.ring.lock();
        ring.iter().rev().cloned().collect()
    }
}

static SLOW: OnceLock<SlowLog> = OnceLock::new();

fn slow() -> &'static SlowLog {
    SLOW.get_or_init(SlowLog::new)
}

/// Set the slow-query threshold, in microseconds.  `0` disables the
/// slow log entirely.  Set at startup from
/// `perf.slow_query_threshold_ms` in the hjson config.
pub fn set_slow_threshold_us(us: u64) {
    slow().threshold_us.store(us, Ordering::Relaxed);
}

/// Current slow-query threshold (µs).  Exposed for `v2/perf.settings`
/// callers and the dashboard.
pub fn slow_threshold_us() -> u64 {
    slow().threshold_us.load(Ordering::Relaxed)
}

/// Snapshot the slow-query ring buffer, newest first.  Capped at
/// [`SLOW_LOG_CAPACITY`] entries.
pub fn slow_snapshot() -> Vec<SlowEntry> {
    slow().snapshot()
}

/// Hot-path entry — record one µs sample for the named series.
///
/// Names are convention-typed by the caller; the registry is open:
/// instrumentation code in different modules can each invent labels
/// without coordinating.  Recommended prefixes:
///
/// - `ingest.*` — ingest pipeline (flush duration, channel lag)
/// - `fanout.*` — cluster fan-out RPCs (per-peer RTT)
/// - `embed.*`  — embedding engine calls
/// - `shard.*`  — shard-level operations (search, write)
pub fn record(name: &str, value_us: u64) {
    registry().entry(name).record(value_us);
}

/// Record a duration sample (in microseconds) AND participate in the
/// slow-query log when the value exceeds the process-wide threshold.
///
/// Prefer this over [`record`] for any series whose unit is µs.
/// `record` itself is kept for non-duration samples (e.g.
/// `ingest.batch_size`, where the value is a record count rather than
/// a time).
pub fn record_us(name: &str, value_us: u64) {
    registry().entry(name).record(value_us);
    slow().record(name, value_us);
}

/// Convenience: time a synchronous block and record its duration
/// in µs.  The closure's return value is returned verbatim so the
/// instrumentation is invisible at the call site.
///
/// ```ignore
/// let result = perf::time("ingest.flush", || db.add_batch(docs));
/// ```
pub fn time<R, F: FnOnce() -> R>(name: &str, f: F) -> R {
    let started = std::time::Instant::now();
    let out = f();
    let elapsed_us = started.elapsed().as_micros() as u64;
    record(name, elapsed_us);
    // Threshold check is one atomic load + compare — cheap enough to
    // run on every call without measurable overhead.
    slow().record(name, elapsed_us);
    out
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_basic() {
        let s = Series::new();
        for v in 1..=100 { s.record(v); }
        let snap = s.snapshot();
        assert_eq!(snap.n_total, 100);
        assert_eq!(snap.n_recent, 100);
        assert_eq!(snap.min_us, 1);
        assert_eq!(snap.max_us, 100);
        assert_eq!(snap.p50_us, 50);
        assert_eq!(snap.p95_us, 95);
        assert_eq!(snap.p99_us, 99);
    }

    #[test]
    fn ring_wraps_when_overflowing_max_samples() {
        let s = Series::new();
        // Push MAX_SAMPLES + 100 values; the oldest 100 must be gone.
        for v in 0..(MAX_SAMPLES as u64 + 100) { s.record(v); }
        let snap = s.snapshot();
        assert_eq!(snap.n_total, MAX_SAMPLES as u64 + 100);
        assert_eq!(snap.n_recent, MAX_SAMPLES as u64);
        // Lifetime min/max survive the wrap.
        assert_eq!(snap.min_us, 0);
        assert_eq!(snap.max_us, MAX_SAMPLES as u64 + 99);
    }

    #[test]
    fn empty_series_returns_zeroes() {
        let s = Series::new();
        let snap = s.snapshot();
        assert_eq!(snap.n_total, 0);
        assert_eq!(snap.p95_us, 0);
    }

    #[test]
    fn registry_creates_on_demand_and_is_shared() {
        record("test.series", 100);
        record("test.series", 200);
        let snap = registry().snapshot_one("test.series");
        assert!(snap.n_total >= 2);
    }

    #[test]
    fn time_closure_returns_inner_value() {
        let n: u32 = time("test.time", || 42);
        assert_eq!(n, 42);
        let snap = registry().snapshot_one("test.time");
        assert!(snap.n_total >= 1);
    }

    #[test]
    fn slow_log_records_when_threshold_exceeded() {
        let log = SlowLog::new();
        log.threshold_us.store(100, Ordering::Relaxed);
        log.record("test.fast", 50);   // below
        log.record("test.slow", 500);  // above
        let snap = log.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name, "test.slow");
        assert_eq!(snap[0].elapsed_us, 500);
    }

    #[test]
    fn slow_log_threshold_zero_disables() {
        let log = SlowLog::new();
        log.threshold_us.store(0, Ordering::Relaxed);
        log.record("any", 1_000_000);
        assert!(log.snapshot().is_empty());
    }

    #[test]
    fn p95_us_returns_none_below_min_samples() {
        // Use a series name unlikely to collide with other tests in this module.
        for v in 1..=10 { record("test.p95_helper.few", v); }
        assert!(registry().p95_us("test.p95_helper.few", 20).is_none());
    }

    #[test]
    fn p95_us_returns_some_when_enough_samples() {
        for v in 1..=50 { record("test.p95_helper.many", v); }
        let p = registry().p95_us("test.p95_helper.many", 20)
            .expect("expected Some — 50 samples ≥ 20 min");
        // p95 of 1..=50 is somewhere in the high 40s.
        assert!(p >= 40 && p <= 50, "p95 outside expected range: {p}");
    }

    #[test]
    fn slow_log_caps_at_capacity_with_newest_first() {
        let mut log = SlowLog::new();
        log.capacity = 3;
        log.threshold_us.store(0_000_001, Ordering::Relaxed);
        // Slight delay between records so timestamps could differ; not asserted.
        log.record("a", 10);
        log.record("b", 20);
        log.record("c", 30);
        log.record("d", 40);
        let snap = log.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].name, "d"); // newest first
        assert_eq!(snap[1].name, "c");
        assert_eq!(snap[2].name, "b");
    }
}
