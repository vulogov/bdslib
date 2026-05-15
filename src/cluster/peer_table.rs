//! In-memory peer registry shared across the gossip task and the v3/cluster.* handlers.
//!
//! Each `Peer` records what we last learned about a remote bdsnode: its
//! identity (`node_id`, `url`), its capabilities (`version`, `embedding_model`),
//! its liveness (`state`, `last_seen`, `miss_count`), and when it claims to
//! have started (`started_at`, used to render uptime in the dashboard).

use parking_lot::RwLock;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerState {
    Alive,
    Suspect,
    Dead,
}

impl PeerState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PeerState::Alive   => "alive",
            PeerState::Suspect => "suspect",
            PeerState::Dead    => "dead",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub node_id:        Uuid,
    pub url:            String,
    /// Wall-clock seconds since epoch we last received a successful response from this peer.
    pub last_seen:      u64,
    pub state:          PeerState,
    pub version:        String,
    pub embedding_model: Option<String>,
    /// Remote uptime epoch seconds (`started_at` field from `cluster.hello`).
    pub started_at:     u64,
    /// Consecutive failed gossip ticks; resets to 0 on every success.
    #[serde(default)]
    pub miss_count:     u32,
}

impl Peer {
    pub fn new(node_id: Uuid, url: String) -> Self {
        Self {
            node_id,
            url,
            last_seen:       0,
            state:           PeerState::Suspect,  // unknown until first ping
            version:         String::new(),
            embedding_model: None,
            started_at:      0,
            miss_count:      0,
        }
    }
}

#[derive(Debug)]
pub struct PeerTable {
    self_id:  Uuid,
    /// This node's own `bind_url`.  Used to reject "ghost self" entries
    /// — see [`upsert`](PeerTable::upsert).
    self_url: String,
    peers:    HashMap<Uuid, Peer>,
}

impl PeerTable {
    pub fn new(self_id: Uuid, self_url: impl Into<String>) -> Self {
        Self { self_id, self_url: self_url.into(), peers: HashMap::new() }
    }

    pub fn self_id(&self) -> Uuid { self.self_id }
    pub fn len(&self) -> usize { self.peers.len() }
    pub fn is_empty(&self) -> bool { self.peers.is_empty() }

    /// Insert a freshly-discovered peer (or update its url+capabilities).
    /// Returns `true` if the peer was new to the table.
    pub fn upsert(&mut self, peer: Peer) -> bool {
        // 1. Never store ourselves — by `node_id` AND by `url`.  The URL
        //    check matters after a `--new`: the node comes back with a
        //    fresh `node_id` but the same `bind_url`, while peers keep
        //    gossiping its *old* identity at that URL.  A node_id-only
        //    check would admit that "ghost self".
        if peer.node_id == self.self_id || same_url(&peer.url, &self.self_url) {
            return false;
        }

        // 2. One identity per URL.  A peer that was `--new`'d comes back
        //    with a fresh `node_id` at the same `bind_url`; gossip then
        //    carries BOTH identities around, and — because the URL
        //    still answers pings — the stale one is `record_alive`'d
        //    every tick and never goes Dead.  The table accumulates
        //    multiple "alive" entries for one real node, so the
        //    rebalancer / fan-out treat them as distinct peers and do
        //    N× redundant work against the same URL.
        //
        //    Collapse same-URL entries to the most-recently-*started*
        //    process: a newer `started_at` is the live node, an older
        //    one at the same URL is a dead identity.  Skipped when the
        //    incoming `started_at` is still 0 (not yet handshaked) so an
        //    uninitialised entry can't evict a known-good one.
        if peer.started_at != 0 {
            let siblings: Vec<(Uuid, u64)> = self
                .peers
                .iter()
                .filter(|(id, p)| **id != peer.node_id && same_url(&p.url, &peer.url))
                .map(|(id, p)| (*id, p.started_at))
                .collect();
            if siblings.iter().any(|&(_, started)| started > peer.started_at) {
                // A strictly-newer identity for this URL already exists
                // — `peer` is the stale one.
                return false;
            }
            for (stale_id, _) in siblings {
                self.peers.remove(&stale_id);
            }
        }

        match self.peers.get_mut(&peer.node_id) {
            Some(existing) => {
                if peer.last_seen >= existing.last_seen {
                    existing.url             = peer.url;
                    existing.version         = peer.version;
                    existing.embedding_model = peer.embedding_model;
                    existing.started_at      = peer.started_at;
                    existing.last_seen       = peer.last_seen;
                    existing.state           = peer.state;
                    existing.miss_count      = peer.miss_count;
                }
                false
            }
            None => {
                self.peers.insert(peer.node_id, peer);
                true
            }
        }
    }

    /// Mark a peer alive after a successful ping/handshake.
    pub fn record_alive(&mut self, id: Uuid) {
        if let Some(p) = self.peers.get_mut(&id) {
            p.state      = PeerState::Alive;
            p.last_seen  = now_secs();
            p.miss_count = 0;
        }
    }

    /// Reconcile a successful ping: the node at the pinged URL answered
    /// as `actual_id`.
    ///
    /// - When `actual_id == pinged_id`, this is just normal alive
    ///   bookkeeping ([`record_alive`](Self::record_alive)).
    /// - When they differ, the node was `--new`'d — the entry we
    ///   pinged is a **dead identity at a still-live URL**, the exact
    ///   thing the [`upsert`](Self::upsert) URL-collapse also targets,
    ///   but caught here *immediately* on direct contact rather than
    ///   waiting for a gossip-merge round.  The stale entry is reaped;
    ///   the actually-responding identity, if already in the table, is
    ///   marked alive (otherwise gossip/hello will add it shortly).
    ///
    /// Returns `true` when a stale entry was reaped.
    pub fn reconcile_ping(&mut self, pinged_id: Uuid, actual_id: Uuid) -> bool {
        if pinged_id == actual_id {
            self.record_alive(pinged_id);
            return false;
        }
        let reaped = self.peers.remove(&pinged_id).is_some();
        if self.peers.contains_key(&actual_id) {
            self.record_alive(actual_id);
        }
        reaped
    }

    /// Bump miss_count after a failed ping.  Caller decides whether to
    /// transition to Suspect/Dead based on the configured timeouts; this
    /// just records the failure.
    pub fn record_miss(&mut self, id: Uuid) {
        if let Some(p) = self.peers.get_mut(&id) {
            p.miss_count = p.miss_count.saturating_add(1);
        }
    }

    /// Run the configured liveness timeouts over every peer.  Peers that
    /// haven't been seen in `suspect_after` transition Alive → Suspect; peers
    /// that haven't been seen in `dead_after` transition to Dead.  Returns
    /// the count of peers that changed state.
    pub fn sweep(&mut self, suspect_after: Duration, dead_after: Duration) -> usize {
        let now = now_secs();
        let mut changes = 0;
        for p in self.peers.values_mut() {
            if p.last_seen == 0 { continue; }
            let age_secs = now.saturating_sub(p.last_seen);
            let new_state = if age_secs >= dead_after.as_secs() {
                PeerState::Dead
            } else if age_secs >= suspect_after.as_secs() {
                PeerState::Suspect
            } else {
                continue;  // still inside grace window
            };
            if p.state != new_state {
                p.state = new_state;
                changes += 1;
            }
        }
        changes
    }

    /// Currently-alive peers (excludes Suspect and Dead).
    pub fn alive(&self) -> Vec<Peer> {
        self.peers.values()
            .filter(|p| p.state == PeerState::Alive)
            .cloned()
            .collect()
    }

    pub fn alive_count(&self) -> usize {
        self.peers.values().filter(|p| p.state == PeerState::Alive).count()
    }

    /// (alive, suspect, dead) tuple — used by `v3/cluster.status`.
    pub fn count_by_state(&self) -> (usize, usize, usize) {
        let mut a = 0; let mut s = 0; let mut d = 0;
        for p in self.peers.values() {
            match p.state {
                PeerState::Alive   => a += 1,
                PeerState::Suspect => s += 1,
                PeerState::Dead    => d += 1,
            }
        }
        (a, s, d)
    }

    /// Pick a random Alive peer for the next gossip tick.  Returns `None`
    /// when no Alive peers exist (Standalone mode).
    pub fn pick_random_alive(&self) -> Option<Peer> {
        let alive: Vec<&Peer> = self.peers.values()
            .filter(|p| p.state == PeerState::Alive)
            .collect();
        let mut rng = rand::thread_rng();
        alive.choose(&mut rng).map(|p| (*p).clone())
    }

    /// Pick a random peer that is **not** currently Alive — i.e. Suspect or
    /// Dead.  Used by the recovery probe so peers marked Dead don't stay
    /// dead forever after a transient outage.  Returns `None` when every
    /// peer is currently Alive.
    pub fn pick_random_non_alive(&self) -> Option<Peer> {
        let candidates: Vec<&Peer> = self.peers.values()
            .filter(|p| p.state != PeerState::Alive)
            .collect();
        let mut rng = rand::thread_rng();
        candidates.choose(&mut rng).map(|p| (*p).clone())
    }

    /// Snapshot every peer (any state) for serialisation or the cluster page.
    pub fn snapshot(&self) -> Vec<Peer> {
        self.peers.values().cloned().collect()
    }

    /// Merge a remote peer view (from `v3/cluster.peers`).  Last-seen wins;
    /// our own node_id is never overwritten.
    pub fn merge_remote(&mut self, remote: Vec<Peer>) -> usize {
        let mut new = 0;
        for p in remote {
            if self.upsert(p) { new += 1; }
        }
        new
    }
}

pub type SharedPeerTable = Arc<RwLock<PeerTable>>;

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Two peer URLs refer to the same endpoint.  Trailing-slash-tolerant
/// — `bind_url` propagates verbatim through gossip, but a stray `/`
/// shouldn't defeat the self / same-URL identity checks.
fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn p(url: &str) -> Peer {
        let mut p = Peer::new(Uuid::now_v7(), url.into());
        p.last_seen = now_secs();
        p.state     = PeerState::Alive;
        p
    }

    #[test]
    fn upsert_skips_self() {
        let me = Uuid::now_v7();
        let mut t = PeerTable::new(me, "http://self");
        let mut self_peer = p("http://x");
        self_peer.node_id = me;
        assert!(!t.upsert(self_peer));
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn upsert_skips_ghost_self_by_url() {
        // A peer with a *different* node_id but our own bind_url — the
        // "ghost self" left behind after a `--new`.  Must be rejected.
        let mut t = PeerTable::new(Uuid::now_v7(), "http://127.0.0.1:9711");
        let ghost = p("http://127.0.0.1:9711/"); // note trailing slash — still rejected
        assert!(!t.upsert(ghost));
        assert_eq!(t.len(), 0);
        // A genuinely different URL is still accepted.
        assert!(t.upsert(p("http://127.0.0.1:9712")));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn upsert_collapses_stale_identities_by_url() {
        // A peer `--new`'d several times: multiple node_ids at one URL,
        // distinguished only by `started_at`.  The table must keep just
        // the most-recently-started identity — the live process.
        let mut t = PeerTable::new(Uuid::now_v7(), "http://self");
        let mut ghost1 = p("http://127.0.0.1:9712"); ghost1.started_at = 1_000;
        let mut ghost2 = p("http://127.0.0.1:9712"); ghost2.started_at = 2_000;
        let mut fresh  = p("http://127.0.0.1:9712"); fresh.started_at  = 9_000;

        assert!(t.upsert(ghost1));
        assert!(t.upsert(ghost2)); // newer → evicts ghost1
        assert_eq!(t.len(), 1);
        assert!(t.upsert(fresh));  // newest → evicts ghost2
        assert_eq!(t.len(), 1);
        assert_eq!(t.snapshot()[0].started_at, 9_000);

        // A late-arriving STALE identity for that URL is rejected.
        let mut late_ghost = p("http://127.0.0.1:9712"); late_ghost.started_at = 500;
        assert!(!t.upsert(late_ghost));
        assert_eq!(t.len(), 1);
        assert_eq!(t.snapshot()[0].started_at, 9_000);

        // A different URL is untouched by the collapse.
        let mut other = p("http://127.0.0.1:9713"); other.started_at = 1;
        assert!(t.upsert(other));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn reconcile_ping_reaps_stale_identity() {
        let mut t = PeerTable::new(Uuid::now_v7(), "http://self");

        // We hold a stale identity for a URL; the URL now answers as a
        // different node_id (the peer was `--new`'d).
        let ghost    = p("http://127.0.0.1:9712");
        let ghost_id = ghost.node_id;
        let real_id  = Uuid::now_v7();
        t.upsert(ghost);
        assert_eq!(t.len(), 1);

        assert!(t.reconcile_ping(ghost_id, real_id)); // mismatch → ghost reaped
        assert_eq!(t.len(), 0);

        // Matching identity → no reap, just alive bookkeeping.
        let keep    = p("http://127.0.0.1:9713");
        let keep_id = keep.node_id;
        t.upsert(keep);
        assert!(!t.reconcile_ping(keep_id, keep_id));
        assert_eq!(t.len(), 1);
        assert_eq!(t.snapshot()[0].state, PeerState::Alive);
    }

    #[test]
    fn alive_filtering() {
        let mut t = PeerTable::new(Uuid::now_v7(), "http://self");
        let mut p1 = p("http://1"); p1.state = PeerState::Alive;
        let mut p2 = p("http://2"); p2.state = PeerState::Suspect;
        let mut p3 = p("http://3"); p3.state = PeerState::Dead;
        t.upsert(p1); t.upsert(p2); t.upsert(p3);
        assert_eq!(t.alive_count(), 1);
        let (a, s, d) = t.count_by_state();
        assert_eq!((a, s, d), (1, 1, 1));
    }

    #[test]
    fn sweep_promotes_suspect_then_dead() {
        let mut t = PeerTable::new(Uuid::now_v7(), "http://self");
        let mut peer = p("http://x");
        peer.last_seen = now_secs() - 500;
        t.upsert(peer);
        let changes = t.sweep(Duration::from_secs(60), Duration::from_secs(300));
        assert_eq!(changes, 1);
        assert_eq!(t.snapshot()[0].state, PeerState::Dead);
    }

    #[test]
    fn record_alive_resets_miss_count() {
        let mut t = PeerTable::new(Uuid::now_v7(), "http://self");
        let peer = p("http://x");
        let id   = peer.node_id;
        t.upsert(peer);
        t.record_miss(id);
        t.record_miss(id);
        t.record_alive(id);
        assert_eq!(t.snapshot()[0].miss_count, 0);
        assert_eq!(t.snapshot()[0].state, PeerState::Alive);
    }
}
