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
        // Never store ourselves — checked by `node_id` AND by `url`.
        // The URL check matters after a `--new`: the node comes back
        // with a fresh `node_id` but the same `bind_url`, while peers
        // keep gossiping its *old* identity at that URL.  A
        // node_id-only check would admit that "ghost self" — and a
        // peer entry pointing at our own URL makes background tasks
        // (rebalancer record-replication, read fan-out) call
        // *ourselves*, which manifests as RPC timeouts and minutes-long
        // rebalancer ticks.
        if peer.node_id == self.self_id
            || peer.url.trim_end_matches('/') == self.self_url.trim_end_matches('/')
        {
            return false;
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
