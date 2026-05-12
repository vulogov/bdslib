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

/// Default prompt for `web.analyze.agg_search` — output of
/// `v?/aggregationsearch`, i.e. two correlated corpora returned for
/// the same query: live telemetry rows AND operational documents
/// (runbooks, postmortems, design notes, …).  The model must
/// **cross-reference** the two sets — that's the whole reason for
/// the page existing — and produce a single coherent picture.
///
/// Each row in the supplied payload carries a synthetic `_kind`
/// field set to either `"telemetry"` or `"document"` so the model
/// (via `json_fingerprint`) can tell them apart inside the prompt.
pub const DEFAULT_AGG_SEARCH_ANALYZE_PROMPT: &str =
    "You are reviewing the output of an aggregated search that returned **two correlated \
     corpora** for the same operator query: live telemetry rows (`_kind=telemetry`) AND \
     operational documents (`_kind=document` — runbooks, postmortems, design notes, …).  \
     Your job is to cross-reference them and produce a single coherent picture, not two \
     unrelated summaries.  Produce a concise analysis covering:\n\
     1. What is the live telemetry actually saying right now — dominant keys, value \
     ranges, anomalies, failure signals?  Be specific; quote keys and timestamps.\n\
     2. What relevant operational knowledge do the matched documents bring — runbook \
     procedures, prior incidents, architecture context, threshold definitions?  Quote \
     the document name and a short verbatim snippet for each citation.\n\
     3. **Cross-reference** — does any document explain, contextualise, or contradict \
     what the telemetry is showing?  Examples: \"runbook X says the canonical fix for \
     the error pattern in row [3] is to bounce the deployment\"; \"postmortem Y describes \
     the same database-lock signature seen in rows [1, 4, 7]\".  This step is the value \
     of the aggregated view — don't skip it.\n\
     4. The most likely coherent story — what is the operator looking at, and what \
     does the evidence (telemetry + docs together) say is going on?\n\
     5. Gaps — what would you want to know that neither corpus contains?  (Frequently: \
     a longer time window, a specific runbook section, or telemetry for a key that's \
     missing.)\n\
     6. One concrete next investigative or remediation step — preferably one the \
     documents already authorise (\"runbook X step 3\") rather than an ad-hoc action.\n\
     \n\
     Quote both telemetry rows (by key + timestamp) and document names verbatim when \
     citing evidence.  Be terse; bullet points are fine.  If one corpus is empty or \
     dominated by noise, say so explicitly rather than padding the answer.";

/// Default prompt for `web.analyze.primary_query_summary` — output
/// of `v?/summary_for_query`, a TextRank-PageRank extract of the
/// text bodies from primary telemetry records that **matched a
/// specific operator query**.  Unlike `primary_summary` (which
/// summarises everything in a time window), this summary is
/// already focused: the operator asked a question and the vector
/// search picked the records that look most relevant to it.
///
/// The model's job is therefore narrower — **answer the question**
/// the operator was asking, using the summary as evidence.  Not
/// "what is the system doing?" but "what does the system say
/// about *this*?".
///
/// Supplied payload: one row tagged `_kind=primary_query_summary`
/// carrying the query, the summary, and the TextRank knobs.
pub const DEFAULT_PRIMARY_QUERY_SUMMARY_ANALYZE_PROMPT: &str =
    "You are reviewing a TextRank-PageRank summary that distills the text bodies of \
     primary telemetry records matching a specific operator query (carried in the \
     `query` field of the supplied row).  The records summarised below are the ones \
     the vector search judged most relevant to that question; the summary is the \
     highest-rank sentences extracted from them.\n\
     \n\
     Your job is to **answer the operator's question** using the summary as evidence \
     — tell them what the system has to say about the topic they searched for, NOT \
     what the system is doing in general.  Produce a concise analysis covering:\n\
     1. **Direct answer** — one or two sentences that take a position based on the \
     summary.  Anchor with verbatim phrasing pulled from the summary.\n\
     2. **Supporting evidence** — quote 2–4 specific summary sentences that back \
     the answer.\n\
     3. **Signals of trouble within the query scope** — failure-flavoured sentences \
     (`error`, `fail`, `timeout`, `panic`, `unauthorized`, `rejected`, …) quoted \
     verbatim, each with a one-line interpretation of the condition it points to.\n\
     4. **Caveats** — places where the summary is thin, ambiguous, or doesn't \
     actually speak to the query topic (e.g. the top-rank sentences are about an \
     adjacent system that the vector search drifted into).  Operators waste time \
     chasing weak retrieval matches; flag them honestly.\n\
     5. One concrete **next investigative step** — usually a tighter vector search \
     for a verbatim phrase from the summary, a per-key drill-down, or a runbook \
     lookup tied to one of the trouble signals.\n\
     \n\
     Quote summary sentences verbatim when citing evidence.  Bullet points are fine; \
     be terse.  If the summary doesn't actually answer the question — for example, \
     the records were retrieved on weak semantic similarity rather than substantive \
     overlap — say so plainly rather than forcing an answer out of unrelated text.";

/// Default prompt for `web.analyze.primary_summary` — output of
/// `v?/summary_for_recent`, a TextRank-PageRank extract of the
/// highest-rank text bodies from primary telemetry records in the
/// lookback window.  Numeric records (`data` is a bare number, or
/// `data["value"]` is numeric) are filtered out upstream — what
/// reaches this prompt is the system's text-emitted operational
/// language: warnings, status lines, error messages, audit notes.
/// The model's job is to **tell the story** that this summary
/// collectively describes — not to summarise the summary, but to
/// interpret it.
///
/// The supplied payload contains exactly one row tagged
/// `_kind=primary_summary` carrying the summary text and the
/// TextRank knobs the operator picked (`max_sentences`,
/// `min_word_len`) so the model can calibrate its confidence.
pub const DEFAULT_PRIMARY_SUMMARY_ANALYZE_PROMPT: &str =
    "You are reviewing a TextRank-PageRank summary of recent text-bearing primary \
     telemetry records.  Each sentence in this summary is one of the highest-rank text \
     bodies the system emitted in the lookback window — `data.value` / `data.raw` \
     strings extracted from records whose `data` wasn't purely numeric.  Your job is to \
     **tell the story** these sentences collectively describe — interpret them, don't \
     just re-summarise them.\n\
     \n\
     Produce a concise analysis covering:\n\
     1. **Headline** — one sentence answering \"what is the system reporting right \
     now?\"  Anchor it with verbatim phrasing pulled from the summary.\n\
     2. **Themes** — group the summary sentences into 2–4 thematic clusters \
     (auth, networking, storage, scheduling, errors, lifecycle events, …).  Describe \
     each in one or two lines.\n\
     3. **Signals of trouble** — call out summary sentences that look like warnings or \
     failures (`error`, `fail`, `timeout`, `panic`, non-2xx HTTP, OOM, certificate, \
     unauthorized, rejected, …).  Quote the snippet verbatim and explain what condition \
     it points to.\n\
     4. **Healthy chatter** — sentences that read as routine / operational noise \
     (heartbeats, periodic status, lifecycle events) so the operator can mentally tune \
     them out.\n\
     5. **Most likely incident or condition** the summary collectively points to, if \
     any.  Cite specific summary sentences as evidence.\n\
     6. One concrete **next investigative step** — usually a vector search for one of \
     the failure-indicator phrases or a per-key drill-down on a record category that \
     stood out.\n\
     \n\
     Quote summary sentences verbatim when citing evidence so the operator can grep / \
     drill down.  Bullet points are fine; be terse.  If the summary is very short or \
     dominated by boilerplate / repetitive lines, say so plainly rather than \
     speculating — sometimes the right answer is \"this window has nothing interesting \
     in it\".";

/// Default prompt for `web.analyze.templates_summary` — output of
/// `v?/textrank.templates` (a TextRank-PageRank extract of the
/// highest-rank drain3 templates) plus the LDA-discovered topic
/// keywords from `v?/topics.all`.  Neither input is a row list; both
/// are derived/condensed views of the same template population.
/// The model's job is to *weave the summary and the keywords into a
/// coherent story* — that's the value this page adds over the raw
/// Templates page.
///
/// The supplied payload contains exactly two row kinds:
/// `_kind=textrank_summary` (one row, the concatenated summary
/// string) and `_kind=topic_keywords` (one row per log key, carrying
/// the LDA top-N keyword list).
pub const DEFAULT_TEMPLATES_SUMMARY_ANALYZE_PROMPT: &str =
    "You are reviewing two complementary derived views of the system's recent log activity:\n\
     - A TextRank summary built from drain3-mined templates (`_kind=textrank_summary`) — a \
     concatenation of the highest-PageRank template sentences in the lookback window.\n\
     - LDA-discovered topic keywords grouped by log key (`_kind=topic_keywords`) — the \
     vocabulary the system is actually using right now.\n\
     \n\
     Your job is to *weave both into one coherent story* — not summarise them again.  An \
     SRE should be able to read your output and immediately know what the system has been \
     doing.  Produce a concise analysis covering:\n\
     1. **Headline** — one sentence answering \"what is the system reporting right now?\"  \
     Use the dominant keywords AND a fragment of the summary to anchor it.\n\
     2. **Themes** — group the summary sentences and keywords into 2–4 themes (auth, \
     networking, storage, scheduling, errors, …).  Describe each in one or two lines, \
     citing the keywords that label it.\n\
     3. **Signals of trouble** — call out summary fragments that look like warnings or \
     failures (`error`, `fail`, `timeout`, `panic`, non-2xx HTTP, OOM, certificate, \
     unauthorized, …).  Quote the fragment verbatim and explain what condition it \
     points to.\n\
     4. **Healthy noise** — patterns the operator can mentally tune out (heartbeats, \
     polling, periodic writes, scheduled jobs).  Listing the key(s) responsible is \
     enough; don't dwell.\n\
     5. **Most likely incident or condition** the slice points to, if any — cite \
     evidence from BOTH the summary AND the keyword set (one without the other is \
     half a story).\n\
     6. One concrete **next investigative step** — usually a vector search for one of \
     the failure-indicator keywords, or drilling into a specific key from the topic list.\n\
     \n\
     Quote summary fragments and keyword tokens verbatim when citing evidence — the \
     operator wants to be able to grep / drill down.  Bullet points are fine; be terse.  \
     If the summary is very short or the keyword set is dominated by stopwords, say so \
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

    /// Default settings for `web.analyze.agg_search`.  The underlying
    /// `v?/aggregationsearch` RPC already caps its two corpora
    /// (~30 telemetry hits, ~10 document hits), so a 50-row total
    /// budget cleanly accommodates the combined payload.
    pub fn agg_search_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_AGG_SEARCH_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.templates_summary`.  The
    /// payload is small (one summary blob + per-key keyword rows),
    /// but operators with large key cardinality may produce a long
    /// topics list — `max_rows` here caps the *topic-keyword rows*
    /// included, not raw templates.  Default 50 comfortably covers
    /// most deployments.
    pub fn templates_summary_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_TEMPLATES_SUMMARY_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.primary_summary`.  The
    /// payload is always exactly one row (the summary itself), so
    /// `max_rows` doesn't gate anything here — it's retained for
    /// schema parity with the other targets and may be used by
    /// future per-key drill-down variants.
    pub fn primary_summary_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_PRIMARY_SUMMARY_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.primary_query_summary`.
    /// Same single-row payload shape as `primary_summary`; the only
    /// difference is the query-focused default prompt.
    pub fn primary_query_summary_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_PRIMARY_QUERY_SUMMARY_ANALYZE_PROMPT.to_owned(),
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
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Analysis → Agg. Search page (`web.analyze.agg_search.*`).
    pub agg_search_analyze: Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Analysis → Templates Summary page
    /// (`web.analyze.templates_summary.*`).
    pub templates_summary_analyze: Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Analysis → Primary Summary page
    /// (`web.analyze.primary_summary.*`).
    pub primary_summary_analyze: Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Analysis → Primary Query Summary page
    /// (`web.analyze.primary_query_summary.*`).
    pub primary_query_summary_analyze: Arc<AnalyzeTargetConfig>,
}

impl AppState {
    pub fn new(
        node_url: String,
        dashboard_refresh_secs: u64,
        cluster_refresh_secs:   u64,
        shared_secret: String,
        logs_analyze:                  AnalyzeTargetConfig,
        metrics_analyze:               AnalyzeTargetConfig,
        templates_analyze:             AnalyzeTargetConfig,
        agg_search_analyze:            AnalyzeTargetConfig,
        templates_summary_analyze:     AnalyzeTargetConfig,
        primary_summary_analyze:       AnalyzeTargetConfig,
        primary_query_summary_analyze: AnalyzeTargetConfig,
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
            logs_analyze:                  Arc::new(logs_analyze),
            metrics_analyze:               Arc::new(metrics_analyze),
            templates_analyze:             Arc::new(templates_analyze),
            agg_search_analyze:            Arc::new(agg_search_analyze),
            templates_summary_analyze:     Arc::new(templates_summary_analyze),
            primary_summary_analyze:       Arc::new(primary_summary_analyze),
            primary_query_summary_analyze: Arc::new(primary_query_summary_analyze),
        }
    }
}
