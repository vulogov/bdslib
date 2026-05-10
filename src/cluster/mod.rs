//! Cluster layer (Phase 1: membership + discovery).
//!
//! When `cluster.enabled = true` in `bds.hjson`, [`ShardsManager`] holds an
//! `Arc<Cluster>` that owns:
//!
//! - this node's stable [`Uuid`] (loaded from `<dbpath>/network/node_id`),
//! - the [`PeerTable`] of known peers,
//! - the parsed [`ClusterConfig`],
//! - the network directory under which all cluster artefacts live.
//!
//! Outbound RPCs are issued by the gossip task spawned in `bdsnode/main.rs`
//! (see `bdsnode/server/cluster.rs`); inbound v3/cluster.* methods are
//! registered in `bdsnode/jsonrpc/cluster.rs`.
//!
//! [`ShardsManager`]: crate::ShardsManager

pub mod config;
pub mod fanout;
pub mod gossip;
pub mod hints;
pub mod hmac_auth;
pub mod peer_table;
pub mod persistence;
pub mod replication;
pub mod rpc_client;
pub mod tombstones;

pub use config::ClusterConfig;
pub use peer_table::{Peer, PeerState, PeerTable, SharedPeerTable};

use crate::common::error::Result;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Operating mode derived from the live peer count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterMode {
    /// Zero alive peers — local operations only.
    Standalone,
    /// 1..=`full_mode_threshold-1` alive peers — best-effort replication.
    Partial,
    /// >=`full_mode_threshold` alive peers — full replication enforced.
    Full,
}

impl ClusterMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClusterMode::Standalone => "standalone",
            ClusterMode::Partial    => "partial",
            ClusterMode::Full       => "full",
        }
    }
}

/// Process-wide cluster state.  Constructed once by `ShardsManager::new` when
/// `cluster.enabled = true`; held by clones of `ShardsManager` via `Arc`.
pub struct Cluster {
    pub node_id:     Uuid,
    pub config:      ClusterConfig,
    pub peers:       SharedPeerTable,
    pub network_dir: PathBuf,
    /// Process start instant.  Used to compute `started_at` for `cluster.hello`.
    pub started_at:  SystemTime,
    /// Resolved fastembed model name, mirrored from `ShardsManager` for
    /// inclusion in `cluster.hello` payloads.  Optional because tests can use
    /// `with_embedding` without naming a model.
    pub embedding_model: RwLock<Option<String>>,
    /// Shared HTTP client used by the gossip loop and by `fanout` for
    /// distributed-read fan-out.  reqwest pools connections per-`Client`,
    /// so reusing one instance keeps peer-call latency low under load.
    pub http: reqwest::Client,
    /// On-disk hinted-handoff queue used by `v3/add` fan-out and replayed
    /// by the cluster background task.
    pub hints: hints::HintStorage,
    /// Tombstone log for fully-replicated stores (docs, signals, scripts).
    /// Read by anti-entropy so deletes don't get resurrected from peers
    /// that haven't yet learned about them.
    pub tombstones: tombstones::TombstoneStorage,
    /// Live counters surfaced through `v2/cluster.peers` and the bdsweb
    /// dashboard so operators can see when the background tasks last ran
    /// and how many entries they touched.
    pub stats: RwLock<ClusterStats>,
}

/// Telemetry for the cluster background loops.  Updated by the gossip
/// task in `bdsnode/server/cluster.rs` and read by `v2/cluster.peers` /
/// `v3/cluster.status` / the bdsweb cluster page.
#[derive(Debug, Clone, Default)]
pub struct ClusterStats {
    /// Wall-clock seconds the most recent hint replay tick finished.
    pub last_hint_tick:        u64,
    /// Hints replayed in that most-recent tick.
    pub last_hint_tick_replayed: u64,
    /// Wall-clock seconds the most recent anti-entropy tick finished.
    pub last_ae_tick:           u64,
    /// Live entries pulled in that most-recent AE tick (across all stores).
    pub last_ae_tick_pulled:    u64,
    /// Tombstones applied in that most-recent AE tick.
    pub last_ae_tick_tombstones: u64,
    /// Tombstones GC'd in that most-recent AE tick.
    pub last_ae_tick_pruned:    u64,
}

impl Cluster {
    /// Construct a `Cluster` for the given dbpath.  Reads or creates
    /// `<dbpath>/network/node_id`, loads `peers.json` if present, and seeds
    /// the in-memory `PeerTable`.
    pub fn init(dbpath: &str, config: ClusterConfig) -> Result<Arc<Self>> {
        let network_dir = persistence::ensure_network_dir(dbpath)?;
        let node_id     = persistence::load_or_init_node_id(&network_dir)?;
        let persisted   = persistence::load_peers(&network_dir).unwrap_or_default();
        let peers       = gossip::build_initial_table(node_id, persisted);
        let hints       = hints::HintStorage::open(&network_dir)?;
        let tombstones  = tombstones::TombstoneStorage::open(&network_dir)?;

        // Per-Client connection pool.  The default timeout is generous
        // (peer_rpc_timeout + 2s grace); individual calls override with
        // the precise deadline they want.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.peer_rpc_timeout_secs.saturating_add(2)))
            .build()
            .map_err(|e| crate::common::error::err_msg(format!("reqwest client build: {e}")))?;

        log::info!(
            "[cluster] initialised node_id={node_id} bind_url={} bootstrap={:?} (table={} peers, hints={}, tombstones={})",
            config.bind_url, config.bootstrap, peers.read().len(),
            hints.len().unwrap_or(0),
            tombstones.len().unwrap_or(0),
        );

        Ok(Arc::new(Self {
            node_id,
            config,
            peers,
            network_dir,
            started_at: SystemTime::now(),
            embedding_model: RwLock::new(None),
            http,
            hints,
            tombstones,
            stats: RwLock::new(ClusterStats::default()),
        }))
    }

    pub fn started_at_unix(&self) -> u64 {
        self.started_at.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed().unwrap_or_default()
    }

    pub fn alive_count(&self) -> usize {
        self.peers.read().alive_count()
    }

    pub fn mode(&self) -> ClusterMode {
        let alive = self.alive_count();
        if alive == 0 { ClusterMode::Standalone }
        else if alive < self.config.full_mode_threshold { ClusterMode::Partial }
        else { ClusterMode::Full }
    }

    /// Mirror the resolved embedding model name from `ShardsManager` so it
    /// can be included in outbound `cluster.hello` payloads without the
    /// cluster module taking a circular dep on `ShardsManager`.
    pub fn set_embedding_model(&self, name: Option<String>) {
        *self.embedding_model.write() = name;
    }

    /// Persist the current peer table to disk.  Errors are logged-and-swallowed
    /// — losing the on-disk cache only delays recovery after a crash, never
    /// breaks the running node.
    pub fn persist_peers_best_effort(&self) {
        let snap = self.peers.read().snapshot();
        if let Err(e) = persistence::save_peers(&self.network_dir, &snap) {
            log::warn!("[cluster] persist peers.json: {e}");
        }
    }
}
