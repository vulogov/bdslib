use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Latest set of dashboard RPC results, populated by the background poller in
/// `main` and consumed by `routes::dashboard::data`.  `None` means the
/// background task has not yet completed its first successful fetch — the
/// dashboard route renders a "Wait" placeholder in that case.
#[derive(Clone, Debug)]
pub struct DashboardSnapshot {
    pub status:   serde_json::Value,
    pub count:    serde_json::Value,
    pub timeline: serde_json::Value,
    pub shards:   serde_json::Value,
}

/// Latest `v2/cluster.peers` snapshot for the /cluster page.  Populated
/// by the background poller in `main` and consumed by
/// `routes::cluster::data`.  `None` means the poller hasn't completed
/// its first successful fetch — the page renders a "Wait" placeholder
/// in that case (same pattern as DashboardSnapshot).
#[derive(Clone, Debug)]
pub struct ClusterSnapshot {
    pub peers: serde_json::Value,
}

/// Cached cluster-mode flag with a 30-second TTL.  Avoids hitting v2/status
/// for every Telemetry / Analysis / RCA page load while still picking up
/// configuration changes promptly.
#[derive(Clone, Debug)]
pub struct ClusterModeCache {
    pub enabled:    bool,
    pub fetched_at: Instant,
}

impl Default for ClusterModeCache {
    fn default() -> Self {
        // `Instant::now() - long_duration` so the first read forces a refresh.
        Self {
            enabled:    false,
            fetched_at: Instant::now() - std::time::Duration::from_secs(3600),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub node_url:     Arc<String>,
    pub http:         reqwest::Client,
    /// Background-poll interval for the cached Dashboard snapshot, in seconds.
    pub dashboard_refresh_secs: u64,
    /// Most-recent Dashboard snapshot collected by the background task.
    pub dashboard_cache: Arc<RwLock<Option<DashboardSnapshot>>>,
    /// Background-poll interval for the cached Cluster snapshot, in seconds.
    pub cluster_refresh_secs: u64,
    /// Most-recent Cluster snapshot collected by the background task.
    pub cluster_cache: Arc<RwLock<Option<ClusterSnapshot>>>,
    /// Cluster-mode flag (cached).  When true, the per-route handlers
    /// route their RPCs through v3/* (cluster-aware) instead of v2/*.
    pub cluster_mode: Arc<RwLock<ClusterModeCache>>,
    /// Cluster shared secret — same value as `cluster.shared_secret`
    /// in `bds.hjson`.  Used by the auth middleware to verify
    /// `bds_session` cookies and by `/admin/users` to HMAC-sign
    /// `v3/user.*` admin calls.  Empty string when bdsweb was started
    /// without `--config` or when `cluster.enabled = false` on the
    /// target node — both cases disable authentication entirely
    /// (open-access mode).
    pub shared_secret: Arc<String>,
    /// 30-second cache of "is the user store empty cluster-wide?".
    /// While true, the auth middleware grants free access so an
    /// operator can hit `/admin/users` to create the first user.
    /// The cache is refreshed on every miss past its TTL.
    pub bootstrap_cache: Arc<RwLock<crate::auth::BootstrapCache>>,
}

impl AppState {
    pub fn new(
        node_url: String,
        dashboard_refresh_secs: u64,
        cluster_refresh_secs:   u64,
        shared_secret: String,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("failed to build HTTP client");
        Self {
            node_url:      Arc::new(node_url),
            http,
            dashboard_refresh_secs,
            dashboard_cache: Arc::new(RwLock::new(None)),
            cluster_refresh_secs,
            cluster_cache:   Arc::new(RwLock::new(None)),
            cluster_mode:    Arc::new(RwLock::new(ClusterModeCache::default())),
            shared_secret:   Arc::new(shared_secret),
            bootstrap_cache: Arc::new(RwLock::new(crate::auth::BootstrapCache::default())),
        }
    }
}
