//! Per-shard bloom filter on record `Uuid`s.
//!
//! Sized at 1 MiB (≈8.4 M bits) per shard which gives ~800 K records
//! at a 1% false-positive rate, or ~2 M records at ~7% FP.  Memory
//! cost is uniform regardless of how many records the shard actually
//! holds — the constant 1 MiB is a deliberate choice: shards roll on
//! a time boundary (default 1 h), so a few thousand to a few hundred
//! thousand records per shard is the realistic range and a single
//! sizing avoids the headache of growable filters.
//!
//! False positives are harmless: the caller falls through to the
//! existing DuckDB id lookup when the bloom says "maybe".  False
//! negatives WOULD be a correctness bug, so [`insert`] is called on
//! every successful record insert and the bloom is repopulated on
//! shard open from the live `id` column.  Deletes are not tracked —
//! their effect is to raise the FP rate slightly, never to produce
//! a false negative.
//!
//! Hashing: each Uuid contributes two 64-bit halves directly (UUIDv7
//! IDs are random enough in the low 64 bits that no additional
//! hashing is needed); the k probe positions are derived via the
//! Kirsch-Mitzenmacher double-hashing trick (`h1 + i*h2 mod m`).

use std::sync::RwLock;
use uuid::Uuid;

/// 1 MiB filter = 1_048_576 bytes = 8_388_608 bits.
/// Power-of-two so the modulus folds into a mask.
const N_BITS:  usize = 8_388_608;
const N_WORDS: usize = N_BITS / 64;
const MASK:    usize = N_BITS - 1;

/// `k = 7` minimises FP at m/n ≈ 10 bits-per-record (target 800 K
/// records).  Smaller k = lower CPU cost, higher FP rate; this is
/// the textbook sweet spot.
const K: u32 = 7;

/// Approximate bloom filter on record UUIDs.  Interior mutability via
/// `RwLock` so a single instance can be safely shared between the
/// add-path (write) and has-records lookup (read).
pub struct IdBloom {
    bits: RwLock<Box<[u64; N_WORDS]>>,
}

impl IdBloom {
    pub fn new() -> Self {
        Self { bits: RwLock::new(Box::new([0u64; N_WORDS])) }
    }

    /// Insert one ID.  Idempotent; re-inserting an existing ID is a
    /// no-op write (already-set bits remain set).
    pub fn insert(&self, id: Uuid) {
        let Ok(mut g) = self.bits.write() else { return; };
        for slot in probe_slots(id) {
            g[slot / 64] |= 1u64 << (slot % 64);
        }
    }

    /// Insert many IDs under a single lock acquisition.  Used by the
    /// shard-open populate pass and by `add_batch`.
    pub fn insert_many(&self, ids: &[Uuid]) {
        if ids.is_empty() { return; }
        let Ok(mut g) = self.bits.write() else { return; };
        for &id in ids {
            for slot in probe_slots(id) {
                g[slot / 64] |= 1u64 << (slot % 64);
            }
        }
    }

    /// Replace the entire bit-array with a freshly populated set.
    /// Used by `Shard::rebuild_indexes`.
    pub fn reset_with(&self, ids: &[Uuid]) {
        let Ok(mut g) = self.bits.write() else { return; };
        for w in g.iter_mut() { *w = 0; }
        for &id in ids {
            for slot in probe_slots(id) {
                g[slot / 64] |= 1u64 << (slot % 64);
            }
        }
    }

    /// `true` when `id` MAY be present (could be a false positive).
    /// `false` is definitive — the ID was never inserted into this
    /// filter.
    pub fn might_contain(&self, id: Uuid) -> bool {
        let Ok(g) = self.bits.read() else { return true; };  // fail-open: caller falls through to DuckDB
        for slot in probe_slots(id) {
            if g[slot / 64] & (1u64 << (slot % 64)) == 0 {
                return false;
            }
        }
        true
    }

    /// `(maybe_present, definitely_absent)` partition of `ids`.  Used
    /// by `has_records_present` to skip the DuckDB query when ALL
    /// queried IDs definitely aren't in this shard.
    pub fn partition(&self, ids: &[Uuid]) -> (Vec<Uuid>, Vec<Uuid>) {
        let mut maybe   = Vec::with_capacity(ids.len());
        let mut absent  = Vec::with_capacity(ids.len());
        let Ok(g) = self.bits.read() else {
            // fail-open
            return (ids.to_vec(), Vec::new());
        };
        for &id in ids {
            let mut all_set = true;
            for slot in probe_slots(id) {
                if g[slot / 64] & (1u64 << (slot % 64)) == 0 {
                    all_set = false;
                    break;
                }
            }
            if all_set { maybe.push(id); } else { absent.push(id); }
        }
        (maybe, absent)
    }
}

impl Default for IdBloom {
    fn default() -> Self { Self::new() }
}

/// Derive the K probe slot indices for `id`.  Kirsch-Mitzenmacher
/// double hashing: `slot_i = (h1 + i * h2) mod m`.
#[inline]
fn probe_slots(id: Uuid) -> impl Iterator<Item = usize> {
    let (h_hi, h_lo) = id.as_u64_pair();
    // h_hi is the primary hash; h_lo (rotated to decorrelate) is the
    // step.  XOR-rotate is a cheap derandomiser for UUIDv7's
    // partially-time-ordered high bits.
    let h1 = h_hi;
    let h2 = h_lo ^ h_hi.rotate_left(17);
    (0..K).map(move |i| {
        let h = h1.wrapping_add((i as u64).wrapping_mul(h2));
        (h as usize) & MASK
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_returns_false_negative_after_insert() {
        let bloom = IdBloom::new();
        let ids: Vec<Uuid> = (0..10_000).map(|_| Uuid::now_v7()).collect();
        bloom.insert_many(&ids);
        for &id in &ids {
            assert!(bloom.might_contain(id), "false negative for {id}");
        }
    }

    #[test]
    fn false_positive_rate_is_low_at_design_load() {
        // 100k IDs is ~1/8 of the design capacity → FP rate should be
        // well under 1%.  Use deterministic v4-style IDs from a
        // counter so the test is reproducible.
        let bloom = IdBloom::new();
        let inserted: Vec<Uuid> = (0..100_000).map(|i| Uuid::from_u128(i)).collect();
        bloom.insert_many(&inserted);
        let probes: Vec<Uuid> = (1_000_000..1_010_000)
            .map(|i| Uuid::from_u128(i))
            .collect();
        let fps = probes.iter().filter(|&&id| bloom.might_contain(id)).count();
        let fp_rate = fps as f64 / probes.len() as f64;
        assert!(fp_rate < 0.01, "FP rate {fp_rate} should be <1% at 100k load");
    }

    #[test]
    fn partition_separates_known_absent() {
        let bloom = IdBloom::new();
        let present: Vec<Uuid> = (0..100).map(|_| Uuid::now_v7()).collect();
        bloom.insert_many(&present);
        // Mix present + 10 fresh IDs the bloom has never seen.
        let mut probes = present[..50].to_vec();
        probes.extend((0..10).map(|i| Uuid::from_u128(10_000_000 + i)));
        let (maybe, absent) = bloom.partition(&probes);
        // All 50 inserted IDs land in `maybe`.
        for id in &present[..50] {
            assert!(maybe.contains(id), "{id} should be in maybe set");
        }
        // The 10 never-seen IDs land in `absent` (FP rate at this load
        // is well under 1%, so collisions are vanishingly unlikely).
        assert!(absent.len() >= 9, "expected ~10 in absent set, got {}", absent.len());
    }

    #[test]
    fn reset_with_clears_and_repopulates() {
        let bloom = IdBloom::new();
        let initial: Vec<Uuid> = (0..1_000).map(|_| Uuid::now_v7()).collect();
        bloom.insert_many(&initial);
        let replacement: Vec<Uuid> = (0..1_000).map(|_| Uuid::now_v7()).collect();
        bloom.reset_with(&replacement);
        // Old set is mostly gone (a few FPs expected — bounded by FP rate).
        let leaked = initial.iter().filter(|&&id| bloom.might_contain(id)).count();
        assert!(leaked < 50, "too many leaked after reset: {leaked}/1000");
        // New set is fully retained (no false negatives).
        for &id in &replacement {
            assert!(bloom.might_contain(id));
        }
    }

    #[test]
    fn empty_bloom_says_no_to_everything() {
        let bloom = IdBloom::new();
        // FP rate on an empty filter is zero (no bits set).
        for i in 0..1000 {
            assert!(!bloom.might_contain(Uuid::from_u128(i)));
        }
    }

    #[test]
    fn partition_fast_path_when_all_absent() {
        let bloom = IdBloom::new();
        let probes: Vec<Uuid> = (0..50).map(|i| Uuid::from_u128(i)).collect();
        let (maybe, absent) = bloom.partition(&probes);
        assert!(maybe.is_empty(), "empty bloom should partition everything to absent");
        assert_eq!(absent.len(), 50);
    }
}
