//! Development-only synthetic-data generator hooks.
//!
//! Powers the `generate_realistic_data` feature: when enabled on a
//! bdsnode, a background tokio task uses
//! [`crate::common::realistic::generate`] to push batches of fake
//! telemetry + log records into the local ingest pipe on a fixed
//! cadence.
//!
//! This module hosts only the process-wide stats record + the
//! "loud banner" helper; the task itself lives in
//! `src/bin/bdsnode/server/dev_data.rs` so it can pull in tokio +
//! the hjson config-parsing surface without polluting the library
//! with bdsnode-specific deps.
//!
//! **Production deployments should leave this disabled.**  Every
//! status surface that consumes [`stats`] emits a loud "SYNTHETIC
//! DATA" warning so operators can never confuse a demo-mode node
//! with a real one.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Process-wide live stats for the dev-data generator.  Mirrors the
/// retention module's pattern: atomic counters behind a `OnceLock`
/// so every status RPC can read them without locking.
pub struct DevDataStats {
    /// `true` once the generator is running.  Read at every
    /// `v2/status` call so the dashboard banner can switch on.
    pub enabled:                AtomicBool,
    /// Lifetime total of records pushed into the ingest pipe.
    pub records_lifetime:       AtomicU64,
    /// Records pushed in the most recent batch.
    pub records_last_batch:     AtomicU64,
    /// Number of batches emitted since process start.
    pub batches_emitted:        AtomicU64,
    /// Unix seconds the most recent batch finished.
    pub last_run_ts:            AtomicU64,
    /// Wall-clock cost of the most recent batch (ms).
    pub last_run_ms:            AtomicU64,
    /// Lifetime count of batches that failed to enqueue
    /// (channel-full, channel-disconnected, etc.).
    pub errors_lifetime:        AtomicU64,
}

impl DevDataStats {
    fn empty() -> Self {
        Self {
            enabled:            AtomicBool::new(false),
            records_lifetime:   AtomicU64::new(0),
            records_last_batch: AtomicU64::new(0),
            batches_emitted:    AtomicU64::new(0),
            last_run_ts:        AtomicU64::new(0),
            last_run_ms:        AtomicU64::new(0),
            errors_lifetime:    AtomicU64::new(0),
        }
    }
}

static STATS: OnceLock<DevDataStats> = OnceLock::new();

/// Borrow the process-wide stats.  Lazy-initialises on first access
/// so library users that never run the generator pay no cost.
pub fn stats() -> &'static DevDataStats {
    STATS.get_or_init(DevDataStats::empty)
}

/// Flip the `enabled` flag.  Called by the bdsnode task at startup
/// (when generation is going to happen) and never flipped back —
/// the dashboard banner stays for the lifetime of the process so
/// operators don't get a false "all real data" snapshot mid-run.
pub fn mark_enabled() {
    stats().enabled.store(true, Ordering::Relaxed);
}

/// Record one generated batch.  Bumps all live counters.
pub fn record_batch(records: usize, took_ms: u64) {
    let s = stats();
    s.records_lifetime  .fetch_add(records as u64, Ordering::Relaxed);
    s.records_last_batch.store(records as u64,     Ordering::Relaxed);
    s.batches_emitted   .fetch_add(1,              Ordering::Relaxed);
    s.last_run_ms       .store(took_ms,            Ordering::Relaxed);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    s.last_run_ts       .store(now, Ordering::Relaxed);
}

/// Record one failed batch — just bumps `errors_lifetime`.
pub fn record_error() {
    stats().errors_lifetime.fetch_add(1, Ordering::Relaxed);
}

/// Convenience predicate for cheap status-block branching.
pub fn is_enabled() -> bool {
    stats().enabled.load(Ordering::Relaxed)
}

/// Multi-line WARN-level banner the bdsnode binary prints at
/// startup when generation is enabled.  Centralised here so the
/// JSON-RPC and bdsweb surfaces can re-render the same message.
pub fn loud_warning_banner() -> &'static str {
"┌──────────────────────────────────────────────────────────────────────┐
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
└──────────────────────────────────────────────────────────────────────┘"
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_batch_bumps_lifetime_and_overwrites_last() {
        // Run before any other test that touches stats.  We can't
        // reset the OnceLock so we just verify the delta increments.
        let before = stats().records_lifetime.load(Ordering::Relaxed);
        record_batch(123, 17);
        record_batch(45,  3);
        let after = stats().records_lifetime.load(Ordering::Relaxed);
        assert!(after - before >= 123 + 45);
        assert_eq!(stats().records_last_batch.load(Ordering::Relaxed), 45);
        assert_eq!(stats().last_run_ms.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn loud_banner_mentions_the_key_phrases() {
        let b = loud_warning_banner();
        assert!(b.contains("SYNTHETIC DATA"));
        assert!(b.contains("NOT REAL"));
        assert!(b.contains("--generate_realistic_data"));
    }
}
