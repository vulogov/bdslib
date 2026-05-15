//! State helpers for the rebalancer:
//!
//! - [`HaveCache`] — process-wide, TTL-bounded `(peer, record) → bool`
//!   cache.  Populated by every successful `has_records` response AND
//!   every successful `replicate_record` (we just shipped that record,
//!   we know the peer has it).  Consulted before issuing probes so the
//!   second sweep through the same records doesn't re-pay the probe
//!   cost.
//!
//! - [`SweepState`] — per-sweep mutable state.  Tracks per-peer probe
//!   timeout counts and the set of peers that have crossed the
//!   skip-threshold for this sweep.  Reset between sweeps.
//!
//! Both are isolated here so the integration in
//! `bdsnode/server/rebalancer.rs` stays focused on flow rather than
//! data-structure plumbing.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────────────
// HaveCache — cross-sweep, process-wide
// ─────────────────────────────────────────────────────────────────────────────

struct CacheEntry {
    has:      bool,
    inserted: Instant,
}

struct CacheInner {
    map:      HashMap<(Uuid, Uuid), CacheEntry>,
    ttl:      Duration,
    capacity: usize,
}

pub struct HaveCache {
    inner: Mutex<CacheInner>,
}

static CACHE: OnceLock<HaveCache> = OnceLock::new();

/// Borrow the process-wide cache.  Lazily initialised with library
/// defaults; `configure()` lets the rebalancer override capacity / TTL
/// from `bds.hjson` at startup.
pub fn cache() -> &'static HaveCache {
    CACHE.get_or_init(|| HaveCache::new(100_000, Duration::from_secs(1800)))
}

/// Reconfigure the live cache (idempotent — safe to call multiple
/// times).  When `capacity = 0` OR `ttl.is_zero()` the cache is
/// effectively disabled: `get` always returns `None` and `insert` is
/// a no-op.
pub fn configure(capacity: usize, ttl: Duration) {
    let c = cache();
    if let Ok(mut g) = c.inner.lock() {
        g.capacity = capacity;
        g.ttl = ttl;
        if capacity == 0 || ttl.is_zero() {
            g.map.clear();
        }
    }
}

impl HaveCache {
    fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                map: HashMap::new(),
                ttl,
                capacity,
            }),
        }
    }

    /// Returns `Some(has)` when the (peer, record) pair has a fresh
    /// entry; `None` when missing or expired.  Expired entries are
    /// not removed here — eviction is amortised via [`evict_expired`].
    pub fn get(&self, peer: Uuid, record: Uuid) -> Option<bool> {
        let g = self.inner.lock().ok()?;
        if g.capacity == 0 || g.ttl.is_zero() { return None; }
        let e = g.map.get(&(peer, record))?;
        if e.inserted.elapsed() < g.ttl {
            Some(e.has)
        } else {
            None
        }
    }

    /// Insert (peer, record) → has.  Replaces any prior entry.  When
    /// the cache is at capacity a random existing entry is dropped
    /// to make room — same evict-random strategy [`JsonCache`] uses.
    pub fn insert(&self, peer: Uuid, record: Uuid, has: bool) {
        let Ok(mut g) = self.inner.lock() else { return; };
        if g.capacity == 0 || g.ttl.is_zero() { return; }
        if g.map.len() >= g.capacity && !g.map.contains_key(&(peer, record)) {
            // HashMap iteration order is non-deterministic; the first
            // key acts as a cheap random victim.  Acceptable for an
            // approximate-LRU; the rebalancer's working set re-warms
            // the next pass.
            if let Some(k) = g.map.keys().next().cloned() {
                g.map.remove(&k);
            }
        }
        g.map.insert((peer, record), CacheEntry { has, inserted: Instant::now() });
    }

    /// Drop every expired entry.  Called by the rebalancer at the
    /// start of each sweep so the cache doesn't grow unboundedly on
    /// long-lived nodes.
    pub fn evict_expired(&self) {
        let Ok(mut g) = self.inner.lock() else { return; };
        let ttl = g.ttl;
        if ttl.is_zero() { return; }
        g.map.retain(|_, e| e.inserted.elapsed() < ttl);
    }

    /// Current entry count — surfaced through logs / status RPCs.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.map.len()).unwrap_or(0)
    }

    /// True when the cache is effectively off (capacity 0 OR ttl 0).
    /// Public for the unit tests and any future status surface; not
    /// part of the hot path.
    #[allow(dead_code)]
    pub fn is_disabled(&self) -> bool {
        self.inner.lock()
            .map(|g| g.capacity == 0 || g.ttl.is_zero())
            .unwrap_or(true)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SweepState — per-scan_pass mutable state
// ─────────────────────────────────────────────────────────────────────────────

/// State that lives for the duration of one rebalancer sweep.  Tracks
/// per-peer probe-timeout counts and the set of peers that have crossed
/// the operator-configured skip threshold.
pub struct SweepState {
    timeouts:           HashMap<Uuid, u32>,
    skip:               HashSet<Uuid>,
    /// 0 disables peer-skip entirely.
    timeout_skip_after: u32,
}

impl SweepState {
    pub fn new(timeout_skip_after: u32) -> Self {
        Self {
            timeouts: HashMap::new(),
            skip:     HashSet::new(),
            timeout_skip_after,
        }
    }

    /// True when this peer has been quarantined for the rest of this
    /// sweep.
    pub fn is_slow(&self, peer: Uuid) -> bool {
        self.skip.contains(&peer)
    }

    /// Note a probe error (timeout, network, rpc).  Returns `true` if
    /// this push the peer over the threshold for the first time (so
    /// the caller can emit a one-time log line).
    pub fn note_timeout(&mut self, peer: Uuid) -> bool {
        if self.timeout_skip_after == 0 { return false; }
        let n = self.timeouts.entry(peer).or_insert(0);
        *n += 1;
        if *n >= self.timeout_skip_after && self.skip.insert(peer) {
            true
        } else {
            false
        }
    }

    /// Count of peers currently on the skip list.
    pub fn skipped_count(&self) -> usize {
        self.skip.len()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn uuid(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    // ── HaveCache ────────────────────────────────────────────────────────────

    #[test]
    fn cache_roundtrip() {
        let c = HaveCache::new(10, Duration::from_secs(10));
        assert_eq!(c.get(uuid(1), uuid(2)), None);
        c.insert(uuid(1), uuid(2), true);
        assert_eq!(c.get(uuid(1), uuid(2)), Some(true));
        c.insert(uuid(1), uuid(2), false);
        assert_eq!(c.get(uuid(1), uuid(2)), Some(false));
    }

    #[test]
    fn cache_expiry() {
        let c = HaveCache::new(10, Duration::from_millis(50));
        c.insert(uuid(1), uuid(2), true);
        assert_eq!(c.get(uuid(1), uuid(2)), Some(true));
        thread::sleep(Duration::from_millis(80));
        assert_eq!(c.get(uuid(1), uuid(2)), None);
    }

    #[test]
    fn cache_evict_expired_clears_stale() {
        let c = HaveCache::new(10, Duration::from_millis(30));
        c.insert(uuid(1), uuid(2), true);
        c.insert(uuid(1), uuid(3), false);
        assert_eq!(c.len(), 2);
        thread::sleep(Duration::from_millis(60));
        c.evict_expired();
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn cache_capacity_evicts_to_make_room() {
        let c = HaveCache::new(2, Duration::from_secs(60));
        c.insert(uuid(1), uuid(1), true);
        c.insert(uuid(1), uuid(2), true);
        assert_eq!(c.len(), 2);
        c.insert(uuid(1), uuid(3), true);
        assert_eq!(c.len(), 2);  // one entry got evicted
    }

    #[test]
    fn cache_disabled_when_capacity_zero() {
        let c = HaveCache::new(0, Duration::from_secs(60));
        assert!(c.is_disabled());
        c.insert(uuid(1), uuid(2), true);
        assert_eq!(c.get(uuid(1), uuid(2)), None);
    }

    #[test]
    fn cache_disabled_when_ttl_zero() {
        let c = HaveCache::new(10, Duration::ZERO);
        assert!(c.is_disabled());
        c.insert(uuid(1), uuid(2), true);
        assert_eq!(c.get(uuid(1), uuid(2)), None);
    }

    // ── SweepState ───────────────────────────────────────────────────────────

    #[test]
    fn sweep_state_skips_after_threshold() {
        let mut s = SweepState::new(2);
        assert!(!s.is_slow(uuid(1)));
        assert_eq!(s.note_timeout(uuid(1)), false); // 1 — under threshold
        assert!(!s.is_slow(uuid(1)));
        assert_eq!(s.note_timeout(uuid(1)), true);  // 2 — crosses, first-time
        assert!(s.is_slow(uuid(1)));
        assert_eq!(s.note_timeout(uuid(1)), false); // 3 — already skipped
    }

    #[test]
    fn sweep_state_zero_threshold_disables() {
        let mut s = SweepState::new(0);
        for _ in 0..100 {
            assert_eq!(s.note_timeout(uuid(1)), false);
        }
        assert!(!s.is_slow(uuid(1)));
        assert_eq!(s.skipped_count(), 0);
    }

    #[test]
    fn sweep_state_independent_peers() {
        let mut s = SweepState::new(2);
        s.note_timeout(uuid(1));
        s.note_timeout(uuid(2));
        assert!(!s.is_slow(uuid(1)));
        assert!(!s.is_slow(uuid(2)));
        s.note_timeout(uuid(1));
        assert!(s.is_slow(uuid(1)));
        assert!(!s.is_slow(uuid(2)));
        assert_eq!(s.skipped_count(), 1);
    }
}
