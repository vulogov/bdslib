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
    /// Ollama model name read from bds.hjson (for display in the Chat UI).
    pub ollama_model: Arc<String>,
    /// Background-poll interval for the cached Dashboard snapshot, in seconds.
    pub dashboard_refresh_secs: u64,
    /// Most-recent Dashboard snapshot collected by the background task.
    pub dashboard_cache: Arc<RwLock<Option<DashboardSnapshot>>>,
    /// Cluster-mode flag (cached).  When true, the per-route handlers
    /// route their RPCs through v3/* (cluster-aware) instead of v2/*.
    pub cluster_mode: Arc<RwLock<ClusterModeCache>>,
}

impl AppState {
    pub fn new(node_url: String, ollama_model: String, dashboard_refresh_secs: u64) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("failed to build HTTP client");
        Self {
            node_url:     Arc::new(node_url),
            http,
            ollama_model: Arc::new(ollama_model),
            dashboard_refresh_secs,
            dashboard_cache: Arc::new(RwLock::new(None)),
            cluster_mode:   Arc::new(RwLock::new(ClusterModeCache::default())),
        }
    }
}
