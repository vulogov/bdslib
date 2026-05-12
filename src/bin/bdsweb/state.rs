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

/// Runtime settings for one "Analyze this!" button.  Sourced from a
/// `web.analyze.<target>.*` block of `bds.hjson`; missing keys fall
/// through to per-target compiled-in defaults (see
/// [`Self::logs_default`] and [`Self::metrics_default`]) so operators
/// who don't care don't have to touch anything.
///
/// The shape is identical for every target — only the default prompt
/// differs.  Adding `rca`, `signals`, … later is one factory method +
/// one route + one route registration.
#[derive(Clone, Debug)]
pub struct AnalyzeTargetConfig {
    /// Per-request timeout for the round-trip bdsweb → bdsnode → LLM.
    /// Default 600 s.  CPU-bound local Ollama on llama3.2 with 50 rows
    /// + auto-bumped num_ctx takes 60–180 s on first call; cached hits
    /// return in <50 ms.  Floor 30 s.
    pub timeout_secs:    u64,
    /// How many search hits to feed into the LLM.  Default 50.
    /// Floor 1, ceiling 500 (anything more usually overflows the model
    /// context window).
    pub max_rows:        usize,
    /// The user-facing prompt template prepended to the rows before
    /// sending to `v4/llm.analyze`.  Operators can rewrite this to
    /// change the analysis style.  Default = the per-target prompt
    /// constant (`DEFAULT_LOGS_ANALYZE_PROMPT`,
    /// `DEFAULT_METRICS_ANALYZE_PROMPT`, …).
    pub prompt_template: String,
}

/// Default prompt for `web.analyze.logs` — operational/textual log
/// records (`message`, `key`, `data`).  Frames the analysis as an SRE
/// reading a log slice: themes, failure patterns, anomalies, root
/// cause, next step.
pub const DEFAULT_LOGS_ANALYZE_PROMPT: &str =
    "You are reviewing a slice of operational log records that an SRE \
     just searched for.  Produce a concise analysis covering:\n\
     1. The dominant theme of these records (what is the system doing right now?).\n\
     2. Any recurring failure / error / warning patterns — group similar events.\n\
     3. Anomalies or outliers that look unusual against the rest of the set.\n\
     4. The most likely root cause if any failures are present, with evidence.\n\
     5. One concrete next investigative step the operator should take.\n\
     \n\
     Quote specific log keys, timestamps, and message snippets verbatim \
     when you cite evidence — the operator wants to be able to grep for \
     them.  Be terse; bullet points are fine.  If the data is too sparse \
     to support a conclusion, say so plainly rather than speculating.";

/// Default prompt for `web.analyze.metrics` — numeric telemetry rows
/// (CPU%, mem%, request rates, latencies, …).  Targeted at numeric
/// reasoning rather than free-text log scanning: ranges, outliers,
/// trends, cross-metric correlation, operational verdict.
pub const DEFAULT_METRICS_ANALYZE_PROMPT: &str =
    "You are reviewing a slice of numeric telemetry records that an SRE \
     just searched for.  Each record carries a key (the metric name) and \
     a value or value-map.  Produce a concise analysis covering:\n\
     1. What metric(s) does this data describe, and what are the typical \
     value ranges seen in this slice?\n\
     2. Numerical highlights — per-key min / max / median where possible; \
     call out any obvious outliers or spikes that fall outside the normal band.\n\
     3. Trend direction — are values rising, falling, oscillating, or stable \
     across the time window?  Quote timestamps that bracket the change.\n\
     4. Cross-metric correlation — when one key spikes, does another spike or \
     drop with it?  Call out coincident timestamps when you find them.\n\
     5. Operational interpretation — does the picture look healthy, capacity-bound, \
     in a failure mode, or just noisy?\n\
     6. One concrete next investigative step the operator should take.\n\
     \n\
     Quote keys, timestamps, and numeric values verbatim when citing evidence — \
     the operator should be able to grep for them.  Bullet points are fine; be \
     terse.  If the data is too sparse to support a numeric conclusion, say so \
     plainly rather than speculating.";

/// Default prompt for `web.analyze.templates` — drain3-mined log
/// templates.  Each record is a recurring log-line pattern where the
/// variable parts (IDs, timestamps, paths, numbers) are replaced by
/// `<*>` placeholders.  Targeted at *pattern* reasoning rather than
/// individual record reasoning: which behaviors does the template
/// mix describe, which look like failure indicators, which are noise.
pub const DEFAULT_TEMPLATES_ANALYZE_PROMPT: &str =
    "You are reviewing a slice of drain3-mined log templates — each record \
     is a recurring log-line pattern where variable parts (IDs, timestamps, \
     paths, numbers) are replaced by `<*>` placeholders.  An SRE just pulled \
     these to understand what the system is doing right now.  Produce a \
     concise analysis covering:\n\
     1. The system behaviors these templates describe — group them into \
     themes (startup / shutdown, networking, authentication, persistence, \
     scheduled jobs, errors, …).\n\
     2. Templates that look like failure or warning indicators — words like \
     `error`, `fail`, `timeout`, `panic`, `unauthorized`, `rejected`, \
     non-2xx HTTP codes, OOM-style messages, certificate / TLS issues.  \
     Quote the template body verbatim and explain what condition it points to.\n\
     3. Templates that look benign / high-volume — heartbeats, polling, \
     health checks, periodic metric writes — so the operator can mentally \
     tune them out.\n\
     4. Templates whose `<*>` wildcards span very different value classes \
     (a hostname in one record, an integer in another) — drain3 may have \
     over-collapsed them and the result is a noisy pattern worth splitting.\n\
     5. The most likely incident or condition the template mix points to, if any.\n\
     6. One concrete next investigative step the operator should take — usually \
     a vector search for one of the failure-indicator templates or a per-key \
     drill-down.\n\
     \n\
     Quote template bodies verbatim (preserving the `<*>` placeholders) when \
     citing evidence so the operator can grep and drill down.  Bullet points \
     are fine; be terse.  If the sample is too small or too homogenous to \
     support a conclusion, say so plainly rather than speculating.";

impl AnalyzeTargetConfig {
    /// Default settings for `web.analyze.logs`.
    pub fn logs_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_LOGS_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.metrics`.  Same numeric
    /// budget as logs; the metric-focused prompt is the only
    /// difference.
    pub fn metrics_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_METRICS_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.templates`.  Templates are
    /// usually denser than raw logs (each represents many lines), so
    /// the row budget is the same — 50 patterns is plenty for the
    /// model to work with.
    pub fn templates_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_TEMPLATES_ANALYZE_PROMPT.to_owned(),
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
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Telemetry → Logs page (`web.analyze.logs.*`).
    pub logs_analyze:    Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Telemetry → Metrics page (`web.analyze.metrics.*`).
    pub metrics_analyze: Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Telemetry → Templates page (`web.analyze.templates.*`).
    pub templates_analyze: Arc<AnalyzeTargetConfig>,
}

impl AppState {
    pub fn new(
        node_url: String,
        dashboard_refresh_secs: u64,
        cluster_refresh_secs:   u64,
        shared_secret: String,
        logs_analyze:      AnalyzeTargetConfig,
        metrics_analyze:   AnalyzeTargetConfig,
        templates_analyze: AnalyzeTargetConfig,
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
            logs_analyze:      Arc::new(logs_analyze),
            metrics_analyze:   Arc::new(metrics_analyze),
            templates_analyze: Arc::new(templates_analyze),
        }
    }
}
