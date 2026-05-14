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

/// Default prompt for `web.analyze.primary_lsa_summary` — output of
/// `v?/summary_lsa_for_recent`, an LSA (Latent Semantic Analysis)
/// summary of text-bearing primary telemetry records.  Unlike
/// TextRank (which picks central sentences via PageRank similarity),
/// LSA decomposes the term-document matrix into `n_concepts` latent
/// dimensions via SVD and picks one sentence per concept.  The
/// summary is therefore **deliberately diverse** — each sentence
/// represents a different topical thread, not the same thread
/// rephrased.
///
/// The model's job is to interpret each thread *and* the
/// relationships between them — including cross-concept
/// correlation when two concepts together imply one underlying
/// condition (e.g. "auth thread + database thread spike together
/// → identity-service hitting its DB").
///
/// Supplied payload: one row tagged `_kind=primary_lsa_summary`
/// carrying the summary, the LSA / TextRank knobs the operator
/// picked (`n_concepts`, `max_sentences`, `min_word_len`) so the
/// model can calibrate confidence.
pub const DEFAULT_PRIMARY_LSA_SUMMARY_ANALYZE_PROMPT: &str =
    "You are reviewing an LSA (Latent Semantic Analysis) summary of recent text-bearing \
     primary telemetry records.  LSA decomposes the term-document matrix into N latent \
     *concepts* via SVD and picks one sentence per concept dimension; the summary is \
     therefore deliberately diverse — each sentence represents a different topical \
     thread the system is touching on right now, not the same thread rephrased.  Your \
     job is to **interpret each thread** and tell the operator what story the system \
     is collectively telling.\n\
     \n\
     Produce a concise analysis covering:\n\
     1. **Headline** — one sentence answering \"what is the system doing across these \
     concepts right now?\"  Anchor it with verbatim phrasing from the summary.\n\
     2. **Per-concept breakdown** — for each summary sentence, identify which topical \
     thread it represents (auth, networking, storage, scheduling, errors, lifecycle, …) \
     and what condition it points to.  Quote each sentence verbatim once.\n\
     3. **Signals of trouble** — concepts that look failure-flavoured (`error`, `fail`, \
     `timeout`, `panic`, `unauthorized`, non-2xx HTTP, OOM, certificate, …).  Spell out \
     what the condition implies.\n\
     4. **Healthy threads** — concepts that read as routine operations / heartbeats / \
     periodic status that the operator can mentally tune out.\n\
     5. **Cross-concept correlation** — when two or more concepts together suggest one \
     underlying condition (e.g. an auth thread AND a database thread point at the same \
     incident).  This step is the value of LSA over TextRank — don't skip it.\n\
     6. **Most likely incident or condition** the concept mix points to, if any, \
     citing evidence from the relevant threads.\n\
     7. One concrete **next investigative step** — usually a tighter vector search for \
     verbatim phrasing from one of the trouble-flavoured concepts.\n\
     \n\
     Quote summary sentences verbatim when citing evidence so the operator can grep / \
     drill down.  Bullet points are fine; be terse.  If a concept dimension turned up \
     a sentence that doesn't actually mean anything (LSA can over-decompose sparse \
     corpora into noisy dimensions), say so honestly — sometimes the right answer is \
     \"concept #3 is noise, ignore it\".";

/// Default prompt for `web.analyze.rca_templates` — output of
/// `v?/rca.templates`, the **template-level** root-cause analyser.
/// Same Jaccard + lead-time machinery as `web.analyze.rca`, but
/// the unit of evidence is a drain3-mined log template (a recurring
/// pattern with `<*>` placeholders where variable parts go) rather
/// than a telemetry key.
///
/// The conceptual difference matters for the prompt: each cause
/// is a *log-line pattern*, not a metric name.  When the prompt
/// asks the model to quote evidence verbatim, it must preserve
/// `<*>` placeholders so the operator can grep / drill down.
/// Otherwise the analytical frame (precursors / consequences /
/// clusters / causal story / confidence / validation) is
/// structurally identical to `web.analyze.rca`.
///
/// Supplied payload: one `_kind=rca_templates_window_stats` row +
/// N `_kind=rca_templates_cause` rows (each with a synthetic
/// `is_precursor` flag derived from lead sign) + M
/// `_kind=rca_templates_cluster` rows.  60/40 split favouring
/// causes; stats row always passes through.
pub const DEFAULT_RCA_TEMPLATES_ANALYZE_PROMPT: &str =
    "You are reviewing the output of a co-occurrence + Jaccard-based root-cause analysis \
     run over **drain3-mined log templates** (not raw log lines, not telemetry keys).  \
     Each template is a recurring log-line pattern in which variable parts (IDs, \
     timestamps, paths, numbers) are replaced by `<*>` placeholders.  The detector \
     ranks templates by how often they co-occur with a FAILURE TEMPLATE (`jaccard`) \
     and reports the average lead time in seconds (`avg_lead_secs`).  **Sign \
     convention:** positive `avg_lead_secs` means the template fired BEFORE the \
     failure (a likely PRECURSOR / trigger pattern), negative means AFTER (a likely \
     CONSEQUENCE / effect pattern).  The detector also reports template CLUSTERS — \
     groups of templates that move together within bucket-sized time windows, with a \
     `cohesion` score.\n\
     \n\
     The leading `_kind=rca_templates_window_stats` row identifies the failure \
     template being analysed and the window context.  Your job is to produce an \
     **in-depth template-level RCA insight** — reason about cause-effect ordering \
     using *pattern co-occurrence* as the unit of evidence.  Template-level RCA is \
     structurally informative: when an entire log-line PATTERN of activity \
     precedes a failure PATTERN, that's stronger evidence than a single coincidence \
     of raw lines.\n\
     \n\
     Produce a structured analysis covering:\n\
     1. **Failure identification** — quote the failure template `body` verbatim \
     (preserving `<*>` wildcards) and characterise what operational behaviour it \
     represents.  If the failure_body is empty in the stats row, the detector \
     auto-picked the worst-correlated template — note this and identify which \
     template was chosen.\n\
     2. **Precursor analysis** — examine causes with **positive** `avg_lead_secs`.  \
     Walk through them by both lead time AND Jaccard strength.  A common pattern: \
     `auth.<*>.failed` appearing 90 seconds before `service.<*>.crash` is a credible \
     trigger chain.  Quote template bodies verbatim including their wildcards.\n\
     3. **Consequence analysis** — examine causes with **negative** `avg_lead_secs`.  \
     These templates fired AFTER the failure.  When the same downstream template \
     recurs as a consequence across many distinct failures, that's a recovery-path \
     or alert-fanout pattern worth identifying explicitly.\n\
     4. **Cluster interpretation** — template clusters often map to subsystems (the \
     templates emitted by one component tend to co-occur).  For each cluster, name \
     its likely subsystem and explain why those member templates belong together.  \
     Clusters with high cohesion (≥ 0.7) that **include the failure template** are \
     particularly informative — they show the failure's neighbourhood.\n\
     5. **The causal story** — assemble a narrative of \"template A → template B → \
     failure template → consequence template\".  Anchor every step with verbatim \
     template bodies (including wildcards) and the Jaccard / lead values that \
     support it.  If the evidence is mixed (multiple plausible precursors), present \
     the alternatives rather than picking one arbitrarily.\n\
     6. **Confidence assessment** — how strongly does the template evidence support \
     the story?  Examples: \"high confidence — single Jaccard 0.85 precursor template \
     with 2-minute lead\"; \"medium confidence — three competing precursor templates \
     at Jaccard 0.4–0.6, no clean winner\"; \"low confidence — no precursor template \
     above Jaccard 0.3, the failure template looks orphaned\".\n\
     7. **Validation steps** — concrete actions the operator can run.  Useful \
     patterns for template-level RCA: vector search for one of the precursor \
     template bodies (with wildcards preserved as verbatim text), filter by the \
     failure template's cluster, drill down on a single instance of the failure \
     template to see surrounding context.\n\
     \n\
     Quote template bodies verbatim throughout — preserve the `<*>` placeholders \
     so the operator can paste them straight back into the Templates search box.  \
     If the evidence doesn't support any clear hypothesis, say so plainly and \
     suggest widening the duration or lowering `min_support` / `jaccard_threshold` \
     — RCA failure is itself useful information and dressing it up as a confident \
     answer wastes the operator's time.";

/// Default prompt for `web.analyze.rca` — output of `v?/rca`, the
/// telemetry root-cause analyser.  Returns two correlated outputs:
/// `probable_causes` (keys ranked by Jaccard co-occurrence with the
/// failure, each with `avg_lead_secs` — positive lead = the key
/// fired BEFORE the failure, a likely precursor; negative = AFTER,
/// a likely consequence) and `clusters` (key groupings that move
/// together within bucket-sized time windows, with a `cohesion`
/// score).
///
/// This is the most analytically demanding target.  The operator
/// asked for **in-depth RCA insight** — reasoning about cause-
/// effect ordering, not a one-paragraph summary.  The prompt walks
/// the model through: failure identification, precursor analysis
/// (positive lead), consequence analysis (negative lead), cluster
/// interpretation, the causal story, a confidence assessment, and
/// concrete validation steps.
///
/// Supplied payload: one `_kind=rca_window_stats` row + N
/// `_kind=rca_cause` rows + M `_kind=rca_cluster` rows.  `max_rows`
/// caps causes + clusters combined (60/40 split favouring causes —
/// they directly answer "what caused the failure?"); the stats row
/// is always included on top.
pub const DEFAULT_RCA_ANALYZE_PROMPT: &str =
    "You are reviewing the output of a co-occurrence + Jaccard-based root-cause analysis \
     (RCA) over recent primary telemetry records.  The detector ranks keys by how often \
     they co-occur with a failure (`jaccard`) and reports the average lead time in \
     seconds (`avg_lead_secs`).  **Sign convention:** positive `avg_lead_secs` means \
     the key fired BEFORE the failure (a likely PRECURSOR / trigger), negative means \
     AFTER (a likely CONSEQUENCE / effect).  The detector also reports key CLUSTERS — \
     groups of keys that move together within bucket-sized windows, with a `cohesion` \
     score (1.0 = always co-occur, 0 = random).\n\
     \n\
     The leading `_kind=rca_window_stats` row identifies the failure being analysed \
     and the window context (event count, key count, time span, RCA knobs).  Your job \
     is to produce an **in-depth RCA insight** — walk through the evidence, reason \
     about cause-effect ordering, and tell the operator the most likely story.  This \
     is NOT a one-paragraph summary; the operator wants you to think.\n\
     \n\
     Produce a structured analysis covering:\n\
     1. **Failure identification** — what failure are we investigating (from the stats \
     row), and what does the window context say (event count, key count, time span)?  \
     If `failure_key` is empty, the detector auto-picked the worst failure — call \
     out which key based on its position in the causes list and note that the \
     analysis is auto-targeted rather than operator-directed.\n\
     2. **Precursor analysis** — examine the top causes with **positive** \
     `avg_lead_secs`.  These preceded the failure.  Walk through them by lead time AND \
     Jaccard strength: high Jaccard + short positive lead = strong trigger candidate; \
     high Jaccard + long lead = upstream condition that took time to propagate; low \
     Jaccard regardless of lead = ambient noise.  Quote the verbatim key names, lead \
     times, and Jaccard scores.\n\
     3. **Consequence analysis** — examine the top causes with **negative** \
     `avg_lead_secs`.  These followed the failure — they don't cause it but show how \
     it propagated to other subsystems.  Useful for blast-radius assessment.  If a \
     consequence shows up in many clusters, the failure cascaded broadly.\n\
     4. **Cluster interpretation** — for each cluster, identify what subsystem or \
     behavioural pattern it represents.  Clusters with high cohesion (≥ 0.7) that \
     **include the failure_key** are particularly informative — they show the \
     failure's neighbourhood and which keys move together with it.  Quote member \
     keys verbatim.\n\
     5. **The causal story** — assemble a narrative of \"X happened → Y happened → \
     failure → Z\".  Anchor every step with verbatim key names and the Jaccard / lead \
     values that support it.  If the evidence is mixed (e.g. multiple plausible \
     precursors), present the alternatives rather than picking one arbitrarily — \
     RCA is not always clean and an honest split is more useful than a false \
     certainty.\n\
     6. **Confidence assessment** — how strongly does the evidence support the story?  \
     Examples: \"high confidence — single Jaccard 0.85 precursor with 2-minute lead\"; \
     \"medium confidence — three competing precursors at Jaccard 0.4–0.6, no clean \
     winner\"; \"low confidence — no precursor above Jaccard 0.3, the failure looks \
     orphaned\".\n\
     7. **Validation steps** — concrete actions the operator can run to confirm or \
     refute the hypothesis.  Useful patterns: log-search for the precursor key in \
     the same time window, per-key metric drill-down on the failure's cluster \
     neighbourhood, runbook lookup tied to the failure category, comparing this \
     window to a known-good baseline.\n\
     \n\
     Quote key names, Jaccard scores, and lead times verbatim throughout.  Bullet \
     points and sub-bullets are fine; structure is more important here than terseness.  \
     If the evidence doesn't support any clear hypothesis (the failure is not well \
     correlated with anything in the window), say so plainly and recommend widening \
     the duration or lowering `min_support` — RCA failure is itself useful information \
     and dressing it up as a confident answer wastes the operator's time.";

/// Default prompt for `web.analyze.knn` — output of `v?/knn`, the
/// k-Nearest-Neighbour clustering analysis.  Returns two related
/// outputs: `clusters` (groups of records bound by vector
/// similarity, each with a `representative` and a `members` list)
/// and `anomalies` (records whose `max_similarity` to any cluster
/// fell below the threshold — singletons).  Plus stats: n_logs, k,
/// anomaly_threshold, n_clusters, n_anomalies.
///
/// The LLM's job is to **interpret the clustering structure** — not
/// re-list the clusters, but tell the operator what each cluster
/// *means* operationally and what the anomalies reveal as
/// singletons.
///
/// Supplied payload: one `_kind=knn_window_stats` row + N
/// `_kind=knn_cluster` rows (each carrying id/size/representative/
/// trimmed members) + M `_kind=knn_anomaly` rows.  `max_rows` caps
/// clusters + anomalies combined with a 60/40 split (clusters take
/// the larger share — each cluster carries denser info per row);
/// the stats row is always included on top.  Member lists inside
/// each cluster are clipped to keep one verbose cluster from
/// crowding out the others.
pub const DEFAULT_KNN_ANALYZE_PROMPT: &str =
    "You are reviewing the output of a k-Nearest-Neighbour clustering analysis over \
     recent primary telemetry records.  The detector groups records by vector \
     similarity into CLUSTERS (each with a `representative` record and a `members` list \
     of similar records) and lists records that didn't fit any cluster as ANOMALIES \
     (with `max_similarity` showing how far they are from the nearest cluster).\n\
     \n\
     The leading `_kind=knn_window_stats` row carries the population context (records \
     scanned, k, threshold, cluster count, anomaly count).  Your job is to \
     **interpret the clustering structure** — not re-list the clusters, but tell the \
     operator what each cluster *means* operationally, which clusters matter, and \
     what the anomalies reveal as singletons.\n\
     \n\
     Produce a concise analysis covering:\n\
     1. **Population context** — using the stats row, characterise the clustering: how \
     many records were scanned, how many clusters were found, what fraction is \
     anomalous?  A large anomaly fraction means the system is producing too much \
     novelty for k-NN to coherently cluster — that's itself a finding worth calling \
     out.\n\
     2. **Cluster themes** — for each cluster, identify what operational behaviour it \
     represents (auth, networking, errors, scheduled jobs, lifecycle events, …).  \
     Quote the cluster's representative text verbatim and reference its size.  Rank \
     clusters by operational relevance — a 3-member error cluster usually matters \
     more than a 200-member heartbeat cluster.\n\
     3. **Failure clusters** — clusters whose representative or members suggest \
     errors, timeouts, panics, unauthorized access, non-2xx HTTP, OOM, certificate \
     issues, …  These are the operator's top priority; quote verbatim.\n\
     4. **Routine clusters** — heartbeats, periodic status writes, polling, scheduled \
     job artefacts.  List them by cluster `[id]` so the operator can mentally tune \
     them out without rechecking each one.\n\
     5. **Anomalies** — singletons that didn't fit any cluster.  Each is either \
     (a) genuinely novel and worth investigating, or (b) just a noisy edge case \
     (a stray timestamped log line, a one-off debug print, …).  Assess each \
     verbatim and call out which category it belongs to.\n\
     6. **Story** — what does the cluster + anomaly mix collectively say about what \
     the system has been doing in this window?  Cite both cluster `[id]` and anomaly \
     `[idx]` references as evidence.\n\
     7. One concrete **next investigative step** — usually a vector search for one \
     of the failure-cluster representatives, a per-key drill-down on a large but \
     non-routine cluster, or lowering the anomaly threshold to surface near-singletons.\n\
     \n\
     Quote cluster representatives and anomaly text verbatim when citing evidence.  \
     Bullet points are fine; be terse.  If the clustering is dominated by routine \
     traffic and the anomalies look like benign noise, say so plainly rather than \
     forcing an incident narrative.";

/// Default prompt for `web.analyze.denoise_recent` — output of
/// `v?/denoise.recent`, the n-gram commonness denoiser.  The
/// detector splits the window into TWO correlated corpora:
/// - `kept` records (commonness < threshold) — the signal.
/// - `removed` records (commonness ≥ threshold) — the noise floor
///   (heartbeats, polling, templated status, retry chatter).
///
/// The LLM's job has two parts: (a) tell the story the kept set
/// collectively describes, and (b) sanity-check the filter by
/// characterising what got removed.  Both halves matter — kept
/// alone misses the false-positive question; removed alone misses
/// the signal.
///
/// Supplied payload: one `_kind=denoise_window_stats` row carrying
/// population stats (n_logs, n_unique_ngrams, threshold, n_kept,
/// n_removed), plus rows tagged `_kind=denoise_kept` and
/// `_kind=denoise_removed`.  `max_rows` caps the **total** kept +
/// removed row count (half reserved for kept so the signal can't
/// be crowded out by a large noise floor); the stats row is always
/// included on top of that.
pub const DEFAULT_DENOISE_ANALYZE_PROMPT: &str =
    "You are reviewing the output of an n-gram commonness denoiser over recent primary \
     telemetry records.  The detector splits the window into two correlated corpora:\n\
     - `_kind=denoise_kept` records have low commonness — they're the *signal*: \
     distinctive lines that don't look like routine boilerplate.\n\
     - `_kind=denoise_removed` records have high commonness — they're the *noise floor*: \
     recurring templated chatter (heartbeats, polling, periodic status, retry storms, …) \
     that the filter judged operationally uninteresting.\n\
     \n\
     The leading `_kind=denoise_window_stats` row carries the population context \
     (records scanned, unique n-grams, threshold, kept vs removed counts).  Your job has \
     **two parts**: tell the operator the story the KEPT records collectively describe, \
     AND sanity-check the filter by characterising what got REMOVED.  Both halves \
     matter — analysing kept alone misses the false-positive question, analysing \
     removed alone misses the signal.\n\
     \n\
     Produce a concise analysis covering:\n\
     1. **Population context** — using the stats row, characterise the split: is the \
     noise floor doing the heavy lifting (most records removed) or is the corpus already \
     mostly signal?  If the kept fraction is unusually high or low compared to a normal \
     window, call that out — it's information in its own right.\n\
     2. **The signal** — what story do the KEPT records collectively tell?  Themes, \
     failure indicators, anomalies inside the kept set.  Quote specific `[idx]` and \
     verbatim text snippets as evidence.\n\
     3. **The noise floor** — characterise what got REMOVED.  Is it plausible boilerplate \
     (heartbeats, scheduled jobs, periodic metric writes) the operator can mentally \
     skip?  Or does any of it look like real signal the filter mistakenly threw away?\n\
     4. **Filter quality check** — flag potential **false positives** (real-signal lines \
     hiding in REMOVED) and **false negatives** (clear boilerplate that survived in KEPT).  \
     These guide threshold tuning — if you see two or three of either kind, suggest \
     raising or lowering the threshold accordingly.\n\
     5. **Most likely incident or condition** based ONLY on the kept set.  Cite specific \
     `[idx]` references as evidence.\n\
     6. One concrete **next investigative step** — usually a vector search for a verbatim \
     phrase from the kept set, a per-key drill-down on a clustering signal source, or a \
     threshold adjustment based on the filter-quality check.\n\
     \n\
     Quote record text verbatim when citing evidence.  Bullet points are fine; be terse.  \
     If the kept set is sparse and the removed set is dominated by uniform boilerplate \
     (i.e. nothing interesting happened in this window), say so plainly rather than \
     forcing a narrative.";

/// Default prompt for `web.analyze.anomaly_recent` — output of
/// `v?/anomaly.recent`, the rarity-ranked anomaly detector.  Each
/// anomaly row carries `rarity` (higher = more anomalous), the
/// record `text`, the `novel_ngrams` that drove the rarity score,
/// and the original `idx`.  Stats accompany the rows: `n_logs`
/// scanned, `n_unique_ngrams`, the `anomaly_threshold` chosen, and
/// `mean_rarity` across the whole window.
///
/// The model's job is to **outline the nature** of the anomalies —
/// not just enumerate them.  Each row already has its rarity score
/// and the n-grams that justified it; what the operator needs is an
/// explanation of WHY the system thought each record was anomalous,
/// whether the anomalies cluster (same key/source/timestamp band →
/// coordinated incident), and which are false positives (rare in
/// the rarity-metric sense but operationally meaningless).
///
/// Supplied payload: one `_kind=anomaly_window_stats` row carrying
/// the population stats (n_logs, n_unique_ngrams, threshold, mean
/// rarity), followed by N `_kind=anomaly` rows.  `max_rows` caps
/// the anomaly rows; stats row always gets through.
pub const DEFAULT_ANOMALY_ANALYZE_PROMPT: &str =
    "You are reviewing the output of a rarity-based anomaly detector run over recent \
     primary telemetry records.  The payload carries one `_kind=anomaly_window_stats` \
     row with the population stats (records scanned, unique n-grams, threshold, mean \
     rarity) and N `_kind=anomaly` rows.  Each anomaly row already has its `rarity` \
     score (higher = more anomalous) and the `novel_ngrams` that drove the score — \
     n-grams that didn't appear elsewhere in the window, which is what made the \
     record stand out.\n\
     \n\
     Your job is to **outline the nature** of these anomalies — explain *why* each \
     one is anomalous, look for patterns across the set, and tell the operator the \
     story.  Do NOT just list the anomalies back; the operator can already see them \
     on the page.  Produce a concise analysis covering:\n\
     1. **Population context** — using the stats row, characterise the corpus: was the \
     window dense or sparse?  Is the mean rarity low (most records look normal) or \
     elevated (the whole window is unusual)?  This frames everything else.\n\
     2. **Themes across anomalies** — group the rows by what makes them rare.  Common \
     groupings: same key/source, same error class, same time band, same novel n-gram \
     family (e.g. all mentioning an unfamiliar hostname or service).  Quote example \
     anomalies verbatim by `[idx]` and `rarity`.\n\
     3. **Severity ranking** — top 1–3 anomalies the operator should look at first.  \
     Be opinionated: rarity score is only one input; combine it with the operational \
     weight of the content (an OOM kill at rarity 0.71 matters more than a one-off \
     debug message at rarity 0.95).\n\
     4. **False-positive candidates** — anomalies that are technically rare (e.g. a \
     scheduled-job artefact, a startup-only banner, a fresh service first-emission) \
     but operationally meaningless.  Flag them so the operator can tune them out.\n\
     5. **Most likely incident or condition** — what story do the meaningful \
     anomalies collectively point to?  Cite specific `[idx]` references as evidence.\n\
     6. One concrete **next investigative step** — usually a vector search for one of \
     the verbatim anomalous strings, a per-key drill-down on a clustering anomaly \
     source, or lowering the threshold to surface near-misses.\n\
     \n\
     Quote anomaly text verbatim when citing evidence.  Bullet points are fine; be \
     terse.  If the anomaly set is dominated by noise (boilerplate variance, single \
     orphaned records) and there's no real incident signal, say so plainly rather \
     than forcing a narrative.";

/// Default prompt for `web.analyze.primary_lsa_query_summary` —
/// output of `v?/summary_lsa_for_query`, the **query-driven** LSA
/// summary of primary telemetry text bodies.  Combines two traits:
///
/// - Query-driven, like `primary_query_summary` — the operator
///   asked a specific question and only records matching it were
///   summarised, so the LLM has to *answer the question*, not tell
///   a general story.
/// - LSA-decomposed, like `primary_lsa_summary` — the summary
///   picks one sentence per latent concept, so each sentence is a
///   different thread inside the query scope.
///
/// The prompt blends both: structure the answer around the LSA
/// concept dimensions (per-thread breakdown), but anchor every step
/// back to the operator's question.  Concept dimensions that don't
/// actually speak to the question should be flagged honestly as
/// off-topic LSA artefacts — pretending they're signal is worse
/// than ignoring them.
///
/// Supplied payload: one row tagged `_kind=primary_lsa_query_summary`
/// carrying query, summary, `n_concepts`, and the TextRank knobs.
pub const DEFAULT_PRIMARY_LSA_QUERY_SUMMARY_ANALYZE_PROMPT: &str =
    "You are reviewing a query-driven LSA (Latent Semantic Analysis) summary of recent \
     text-bearing primary telemetry records.  Two things distinguish this input from \
     the other summary views:\n\
     - The operator already asked a specific question (carried in the row's `query` \
     field) — only records matching that question via vector search were summarised.\n\
     - LSA decomposed the resulting text into N latent *concepts* via SVD and picked \
     one sentence per concept dimension.  Each summary sentence is therefore a \
     different topical thread *inside the operator's question*.\n\
     \n\
     Your job is to **answer the operator's question** using the per-concept structure \
     of the summary as evidence — not to tell a general story, and not to summarise \
     the summary.  Produce a concise analysis covering:\n\
     1. **Direct answer** — one or two sentences that take a position on the operator's \
     question, anchored with verbatim phrasing pulled from the summary.\n\
     2. **Per-concept breakdown** — for each summary sentence, name the topical thread \
     it represents and how it relates to the operator's question.  Quote each sentence \
     verbatim once.  If a concept is clearly off-topic (LSA over-decomposing or vector \
     search drifting), flag it as such — don't force it to fit.\n\
     3. **Signals of trouble within the query scope** — concepts that look failure- \
     flavoured (`error`, `fail`, `timeout`, `panic`, `unauthorized`, non-2xx HTTP, OOM, \
     certificate, …).  Quote verbatim and explain the condition.\n\
     4. **Cross-concept correlation** — when two or more concepts together imply one \
     underlying condition that answers the question.  This is the unique value LSA \
     adds over TextRank for query-driven analysis; don't skip it.\n\
     5. **Caveats** — if the retrieved records or the LSA decomposition don't actually \
     speak to the question, say so plainly.  Operators waste time chasing weak \
     semantic matches; flag them honestly rather than forcing an answer out of \
     unrelated text.\n\
     6. One concrete **next investigative step** — usually a tighter vector search \
     for a verbatim phrase from one of the on-topic concepts, a per-key drill-down, \
     or a runbook lookup tied to a trouble signal.\n\
     \n\
     Quote summary sentences verbatim when citing evidence so the operator can grep / \
     drill down.  Bullet points are fine; be terse.  If the summary truly doesn't \
     answer the question, that is a valid answer — say so plainly rather than padding.";

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

/// Default prompt for `web.analyze.perf` — performance series + slow-log
/// snapshot from `v2/perf` and `v2/perf.slow_queries`.  bdslib is a
/// **sharded, mixed-engine** store: every record traverses ONNX embed
/// → DuckDB insert → Tantivy commit → HNSW upsert, with time-partitioned
/// shards routed by the record's `timestamp` field.  Cluster reads
/// (v3/*) fan out to every Alive peer; writes replicate to either a
/// random subset (sharded stores: telemetry) or all peers (replicated
/// stores: docs/signals/scripts/users/llm_cache).
///
/// The prompt teaches the model the series taxonomy so it can correctly
/// attribute latency to the right engine.  All values are µs.
pub const DEFAULT_PERF_ANALYZE_PROMPT: &str =
    "You are diagnosing the performance of a bdslib node — a sharded, \
     mixed-engine telemetry store (DuckDB + Tantivy FTS + HNSW vector \
     index + fastembed ONNX) running standalone or as part of a \
     gossip-based cluster.  All times are in microseconds.\n\
     \n\
     Series taxonomy — use this to attribute latency correctly:\n\
     \n\
     - `ingest.flush`              — wall-clock of one `ShardsManager::add_batch` \
       call.  Sums embed_batch + DuckDB insert + Tantivy commit + HNSW upsert + \
       optional drain mining, across every shard touched.  This is the dominant \
       cost of ingest.\n\
     - `ingest.lag`                — time from the FIRST doc of a batch arriving \
       in the channel to its flush.  Bounded by `pipe_timeout_ms` when traffic is \
       light; expect it to track that ceiling on quiet nodes.\n\
     - `ingest.batch_size`         — number of records per flush (NOT a duration). \
       Approaching `pipe_batch_size` means the flusher is saturating; well below \
       it means traffic is slow / latency-driven.\n\
     - `embed.hit` / `embed.miss`  — query-embedding cache hits vs ONNX inference \
       misses.  Hits run in <10 µs; misses run ~2–15 ms (CPU-bound).  Low hit \
       ratio on a dashboard workload indicates queries vary too much for the cache.\n\
     - `shard.vector_precomputed` / `shard.vector_scored_precomputed` — per-shard \
       HNSW search + MMR rerank.  Linear in shard count for a search; spike \
       suggests cold HNSW or a shard with degraded recall.\n\
     - `shard.fts_scored` / `shard.fts_with_ts` — per-shard Tantivy BM25 search. \
       Tantivy is mmap-based; cold cache → large spike on first read after \
       startup.\n\
     - `fanout.peer.<node_id>`     — RTT of one v3/* read-fan-out RPC to one peer. \
       Adaptive timeout (when enabled) clamps to `min(peer_rpc_timeout, p95 × 3)`. \
       Persistent high p95 → that peer is sick or WAN is degraded.\n\
     - `fanout.method.<m>`         — RTT aggregated across peers per RPC method.\n\
     - `replicate.peer.<node_id>` / `replicate.method.<m>` — write replication RTT \
       (one entry per peer per replicated write).  Distinct from fanout because \
       writes go through a replication pool, not the read fan-out.\n\
     \n\
     Produce a concise analysis covering:\n\
     1. **Headline verdict** — is the node healthy, slow on ingest, slow on \
        reads, slow on cluster, or showing a specific engine bottleneck?  One \
        line.\n\
     2. **Hot-path attribution** — for the slowest series, name the engine \
        responsible (ONNX / DuckDB / Tantivy / HNSW / network).  Cite the \
        specific series name and its p95 in µs.\n\
     3. **Cluster shape** — fan-out skew across peers (one slow peer or all?), \
        replicate vs fanout disparity, evidence of partial reads.  Skip if not \
        in cluster mode (no `fanout.*` / `replicate.*` series present).\n\
     4. **Cache effectiveness** — `embed.hit` / (`embed.hit` + `embed.miss`) \
        ratio; low ratio means dashboard queries vary or cache is undersized.\n\
     5. **Slow-query log** — name the top 1–3 outliers, identify which engine \
        owns each, and call out whether they're cluster-related, ingest-related, \
        or a one-off spike.\n\
     6. **Concrete next step** — one specific action: raise `pipe_flushers`, \
        bump `embedding_cache_size`, run `bdscmd retention-sweep`, investigate \
        peer X, etc.  Cite the bds.hjson knob name.\n\
     \n\
     Be terse; bullet points are fine.  Quote specific series names and p95 \
     values verbatim when citing evidence.  If the sample is too small \
     (most series have n_recent < 20) say so plainly — percentiles below \
     that threshold are noise.";

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

    /// Default settings for `web.analyze.primary_lsa_summary`.  Same
    /// single-row payload shape as the other summary targets; the
    /// default prompt is LSA-aware (per-concept breakdown,
    /// cross-concept correlation).
    pub fn primary_lsa_summary_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_PRIMARY_LSA_SUMMARY_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.primary_lsa_query_summary`.
    /// Combines query-driven semantics (answer the operator) with
    /// LSA multi-concept structure (per-thread breakdown).
    pub fn primary_lsa_query_summary_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_PRIMARY_LSA_QUERY_SUMMARY_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.anomaly_recent`.  Row-list
    /// target; `max_rows` caps the anomaly rows handed to the model
    /// (the one stats row is always included on top of that, so the
    /// LLM never loses population context).
    pub fn anomaly_recent_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_ANOMALY_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.denoise_recent`.  Two-corpus
    /// target: `max_rows` caps the **total** kept + removed row count
    /// fed to the LLM (with half reserved for kept so a noisy window
    /// can't drown out the signal), plus one always-included stats
    /// row on top.
    pub fn denoise_recent_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_DENOISE_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.knn`.  Two-output target:
    /// `max_rows` caps clusters + anomalies combined (60/40 split in
    /// favour of clusters, slack redistributed); members lists
    /// inside each cluster are clipped separately so one verbose
    /// cluster can't crowd out the rest.
    pub fn knn_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_KNN_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.rca`.  Two-output target:
    /// `max_rows` caps causes + clusters combined with a 60/40 split
    /// favouring causes (they directly answer the "what caused it?"
    /// question); stats row always passes through on top.  Default
    /// max_rows of 50 is comfortable for typical RCA output sizes
    /// (15–25 ranked causes + 5–15 clusters).
    pub fn rca_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_RCA_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.rca_templates`.  Same shape
    /// as `rca_default()` — only the default prompt differs (tuned
    /// for template-level reasoning with `<*>` wildcard preservation).
    pub fn rca_templates_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        50,
            prompt_template: DEFAULT_RCA_TEMPLATES_ANALYZE_PROMPT.to_owned(),
        }
    }

    /// Default settings for `web.analyze.perf`.  The payload is small
    /// (typically 5–30 series + up to 100 slow-log entries), so 200
    /// "rows" comfortably accommodates both blocks.  Longer timeout
    /// because the prompt asks for engine-attribution reasoning that
    /// some hosted models take a few seconds to produce.
    pub fn perf_default() -> Self {
        Self {
            timeout_secs:    600,
            max_rows:        200,
            prompt_template: DEFAULT_PERF_ANALYZE_PROMPT.to_owned(),
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
    /// Whether the `bds_session` cookie is issued with the `Secure`
    /// attribute (`web.secure_cookies`).  Resolved at startup: an
    /// explicit config value wins; otherwise it defaults to `false`
    /// for a loopback bind (dev / HTTP-only) and `true` for any other
    /// bind address.  See security finding H2.
    pub secure_cookies: bool,
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
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Analysis → Primary LSA Summary page
    /// (`web.analyze.primary_lsa_summary.*`).
    pub primary_lsa_summary_analyze: Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Analysis → Primary LSA Query Summary page
    /// (`web.analyze.primary_lsa_query_summary.*`).
    pub primary_lsa_query_summary_analyze: Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Analysis → Detect Anomalies page (`web.analyze.anomaly_recent.*`).
    pub anomaly_recent_analyze: Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Analysis → Denoise page (`web.analyze.denoise_recent.*`).
    pub denoise_recent_analyze: Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Analysis → k-NN page (`web.analyze.knn.*`).
    pub knn_analyze: Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the RCA → Telemetry RCA page (`web.analyze.rca.*`).
    pub rca_analyze: Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the RCA → Templates RCA page (`web.analyze.rca_templates.*`).
    pub rca_templates_analyze: Arc<AnalyzeTargetConfig>,
    /// Operator-configurable knobs for the "Analyze this!" button on
    /// the Administration → Performance page (`web.analyze.perf.*`).
    pub perf_analyze: Arc<AnalyzeTargetConfig>,
}

impl AppState {
    pub fn new(
        node_url: String,
        dashboard_refresh_secs: u64,
        cluster_refresh_secs:   u64,
        shared_secret: String,
        secure_cookies: bool,
        logs_analyze:                      AnalyzeTargetConfig,
        metrics_analyze:                   AnalyzeTargetConfig,
        templates_analyze:                 AnalyzeTargetConfig,
        agg_search_analyze:                AnalyzeTargetConfig,
        templates_summary_analyze:         AnalyzeTargetConfig,
        primary_summary_analyze:           AnalyzeTargetConfig,
        primary_query_summary_analyze:     AnalyzeTargetConfig,
        primary_lsa_summary_analyze:       AnalyzeTargetConfig,
        primary_lsa_query_summary_analyze: AnalyzeTargetConfig,
        anomaly_recent_analyze:            AnalyzeTargetConfig,
        denoise_recent_analyze:            AnalyzeTargetConfig,
        knn_analyze:                       AnalyzeTargetConfig,
        rca_analyze:                       AnalyzeTargetConfig,
        rca_templates_analyze:             AnalyzeTargetConfig,
        perf_analyze:                      AnalyzeTargetConfig,
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
            secure_cookies,
            bootstrap_cache: Arc::new(RwLock::new(crate::auth::BootstrapCache::default())),
            logs_analyze:                      Arc::new(logs_analyze),
            metrics_analyze:                   Arc::new(metrics_analyze),
            templates_analyze:                 Arc::new(templates_analyze),
            agg_search_analyze:                Arc::new(agg_search_analyze),
            templates_summary_analyze:         Arc::new(templates_summary_analyze),
            primary_summary_analyze:           Arc::new(primary_summary_analyze),
            primary_query_summary_analyze:     Arc::new(primary_query_summary_analyze),
            primary_lsa_summary_analyze:       Arc::new(primary_lsa_summary_analyze),
            primary_lsa_query_summary_analyze: Arc::new(primary_lsa_query_summary_analyze),
            anomaly_recent_analyze:            Arc::new(anomaly_recent_analyze),
            denoise_recent_analyze:            Arc::new(denoise_recent_analyze),
            knn_analyze:                       Arc::new(knn_analyze),
            rca_analyze:                       Arc::new(rca_analyze),
            rca_templates_analyze:             Arc::new(rca_templates_analyze),
            perf_analyze:                      Arc::new(perf_analyze),
        }
    }
}
