//! In-process sliding-window per-key rate limiter.
//!
//! Used by `v3/user.authenticate` to throttle brute-force login
//! attempts by username — keyed on the **username** (not the source
//! IP) because attackers typically rotate IPs but stay focused on a
//! handful of high-value accounts.  The bdsweb side gets a separate
//! per-IP tower_governor layer on `POST /login`; the two together
//! make a credible defence in depth.
//!
//! ## Algorithm
//!
//! Per key we keep a `VecDeque<Instant>` of recent hit timestamps,
//! trimmed to the trailing window at every `try_acquire`.  The
//! limiter rejects when the trimmed deque length already equals or
//! exceeds the cap.  Per-key state is held under a single global
//! `Mutex<HashMap<…>>`; the critical section is the trim + push and
//! is microseconds-short, so coarse locking is fine here.  This
//! deliberately is NOT a leaky-bucket — it's a fixed-quota sliding
//! window, simplest possible thing that matches the operator's
//! mental model of "N per minute".
//!
//! GC: keys with an empty deque are dropped during `try_acquire` for
//! that key.  Long-idle keys never see another call so their entries
//! get cleaned out by `prune_idle` (called periodically by callers
//! that care; not auto-scheduled).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Process-wide rate limiter.  Cheap to construct (no syscalls); the
/// mutex is uncontended in normal operation.
#[derive(Default)]
pub struct RateLimiter {
    inner: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
    pub fn new() -> Self { Self::default() }

    /// Try to admit a hit against `key`.  Returns `true` when the
    /// caller is within budget, `false` when over.  `max_per_window`
    /// is the hit cap (e.g. 10); `window` is the trailing duration
    /// (typically 60 seconds).
    ///
    /// When `max_per_window == 0` the limiter is disabled: every
    /// call returns `true` and no state is recorded.  This is the
    /// "rate limiting off" opt-out (`auth_rate_limit_per_minute: 0`
    /// in `bds.hjson`).
    pub fn try_acquire(&self, key: &str, max_per_window: u32, window: Duration) -> bool {
        if max_per_window == 0 {
            return true;
        }
        let now = Instant::now();
        let cutoff = now.checked_sub(window).unwrap_or(now);
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        let entry = map.entry(key.to_owned()).or_default();
        // Trim expired entries — sliding window.
        while entry.front().map(|t| *t < cutoff).unwrap_or(false) {
            entry.pop_front();
        }
        if (entry.len() as u32) >= max_per_window {
            return false;
        }
        entry.push_back(now);
        true
    }

    /// Drop any key whose deque has been empty (or is fully expired)
    /// for longer than `window`.  Bounds memory growth when many
    /// distinct usernames are tried briefly then abandoned.  Cheap
    /// to call on a background tick; safe but unnecessary in tests.
    pub fn prune_idle(&self, window: Duration) {
        let cutoff = Instant::now().checked_sub(window).unwrap_or_else(Instant::now);
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, entry| {
            while entry.front().map(|t| *t < cutoff).unwrap_or(false) {
                entry.pop_front();
            }
            !entry.is_empty()
        });
    }

    /// Diagnostic: how many distinct keys are currently tracked.
    pub fn key_count(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn allows_calls_within_budget() {
        let r = RateLimiter::new();
        for _ in 0..5 {
            assert!(r.try_acquire("alice", 5, Duration::from_secs(60)));
        }
    }

    #[test]
    fn rejects_call_over_budget() {
        let r = RateLimiter::new();
        for _ in 0..3 { assert!(r.try_acquire("alice", 3, Duration::from_secs(60))); }
        assert!(!r.try_acquire("alice", 3, Duration::from_secs(60)),
            "4th call within window must be rejected");
    }

    #[test]
    fn keys_are_independent() {
        let r = RateLimiter::new();
        for _ in 0..3 { assert!(r.try_acquire("alice", 3, Duration::from_secs(60))); }
        // bob has his own bucket.
        assert!(r.try_acquire("bob", 3, Duration::from_secs(60)));
    }

    #[test]
    fn zero_budget_disables_limit() {
        let r = RateLimiter::new();
        for _ in 0..1_000 {
            assert!(r.try_acquire("alice", 0, Duration::from_secs(60)),
                "max=0 must admit unconditionally");
        }
        assert_eq!(r.key_count(), 0, "max=0 must not record state");
    }

    #[test]
    fn window_slides_so_old_hits_expire() {
        let r = RateLimiter::new();
        let window = Duration::from_millis(100);
        for _ in 0..3 { assert!(r.try_acquire("alice", 3, window)); }
        assert!(!r.try_acquire("alice", 3, window), "over budget");
        sleep(Duration::from_millis(120));
        assert!(r.try_acquire("alice", 3, window),
            "after window: bucket should be empty again");
    }

    #[test]
    fn prune_idle_drops_emptied_keys() {
        let r = RateLimiter::new();
        let window = Duration::from_millis(50);
        let _ = r.try_acquire("ghost", 5, window);
        assert_eq!(r.key_count(), 1);
        sleep(Duration::from_millis(80));
        r.prune_idle(window);
        assert_eq!(r.key_count(), 0, "idle key dropped after window");
    }
}
