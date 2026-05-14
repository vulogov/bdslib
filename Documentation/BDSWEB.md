# bdsweb — bdsnode Web Interface

`bdsweb` is a dark-themed web UI for `bdsnode`. It connects to a running
`bdsnode` instance via its JSON-RPC 2.0 API and exposes seven pages covering
system status, semantic search over telemetry and logs, document retrieval,
trend analysis, and an interactive BUND scripting workbench.

---

## Table of Contents

1. [Starting bdsweb](#1-starting-bdsweb)
2. [Global Options](#2-global-options)
3. [Environment Variables](#3-environment-variables)
4. [Navigation](#4-navigation)
5. [Dashboard](#5-dashboard)
5b. [Cluster](#5b-cluster)
6. [Telemetry Search](#6-telemetry-search)
7. [Log Search](#7-log-search)
8. [Document Search](#8-document-search)
9. [Aggregated Search](#9-aggregated-search)
10. [Trends](#10-trends)
11. [Bund Workbench](#11-bund-workbench)
11b. [Help pane](#11b-help-pane)
12. [Common UI Patterns](#12-common-ui-patterns)
13. [Authentication](#13-authentication)
14. [LLM — Chat + Administration](#14-llm--chat--administration)
15. [Resilience & health](#15-resilience--health)
16. [TLS / HTTPS](#16-tls--https)

---

## 1. Starting bdsweb

```bash
bdsweb [OPTIONS]
```

`bdsweb` must be able to reach a running `bdsnode` process. Start `bdsnode`
first, then launch `bdsweb`.

**Minimal start (all defaults):**
```bash
bdsweb
# Binds to http://127.0.0.1:8080, connects to bdsnode at http://127.0.0.1:9000
```

**Custom host/port and remote node:**
```bash
bdsweb --host 0.0.0.0 --port 8888 --node http://prod-node.internal:9000
```

---

## 2. Global Options

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--host <ADDR>` | | `127.0.0.1` | Address to bind the HTTP server |
| `--port <PORT>` | `-p` | `8080` | TCP port to listen on |
| `--node <URL>` | `-n` | `http://127.0.0.1:9000` | bdsnode JSON-RPC endpoint |
| `--config <PATH>` | `-c` | — | Path to `bds.hjson`.  Supplies the auth shared secret plus the `web.*` keys — cookie / proxy-trust / TLS (see [§ 16](#16-tls--https)) — and the dashboard/cluster refresh intervals.  Also honours env `BDS_CONFIG`.  Without it, bdsweb runs in open-access mode. |
| `--verbose <LEVEL>` | | `1` | Log verbosity: 0 = warn, 1 = info, 2 = debug |

> **Open-access bind guard.** When bdsweb starts without an auth
> shared secret (no `--config`, or `cluster.enabled = false`) it
> **refuses to start** if `--host` is not a loopback address — an
> unauthenticated admin + Bund-eval surface must never face the
> network.  Bind `127.0.0.1` for local dev, or configure
> `cluster.shared_secret` to enable authentication.

---

## 3. Environment Variables

| Variable | Equivalent flag | Example |
|----------|-----------------|---------|
| `BDSNODE_URL` | `--node` | `http://prod-node.internal:9000` |
| `BDS_CONFIG` | `--config` | `/etc/bdslib/bds.hjson` |

When `BDSNODE_URL` is set, `--node` is not required.  Likewise
`BDS_CONFIG` is an alternative to `--config`.

---

## 4. Navigation

The sticky navigation bar at the top of every page contains the
following items.  Left-side links are content pages; an
`Administration` dropdown and a final `Help` link are pushed to the
right edge of the bar via `margin-left:auto` on the first
right-floating element.

| Side | Label | Path | Purpose |
|------|-------|------|---------|
| left  | Dashboard      | `/`           | System health and shard overview |
| left  | Telemetry      | `/telemetry`  | Semantic search over telemetry records |
| left  | Logs           | `/logs`       | Semantic search over log entries + LDA topics |
| left  | Documents      | `/docs`       | Knowledge-base document retrieval |
| left  | Agg. Search    | `/search`     | Combined telemetry + document search |
| left  | Trends         | `/trends`     | Statistical analysis and time-series charts |
| left  | Signals        | `/signals`    | Emit and search named events |
| left  | Bund           | `/bund`       | Interactive BUND scripting workbench |
| left  | Chat           | `/chat`       | Provider-aware RAG chat |
| right | Administration | `/admin/*`    | User management + LLM admin (dropdown) |
| right | **Help**       | `/help`       | Docstore-backed Q&A — see [§ 11b](#11b-help-pane) |

The active page is highlighted in blue.

---

## 5. Dashboard

**Path:** `GET /`

A read-only health page.  Renders from a cached snapshot maintained
by a background poller in bdsweb — page loads never block on bdsnode
RPCs.  Auto-refreshes via HTMX every `dashboard_refresh_secs` (config
key in `bds.hjson`; defaults to 30 s).  A **Reload** button at the
top-right forces a live fetch + cache write through `/dashboard/refresh`.

Routes:

| Route                  | Method | Purpose                                                          |
|------------------------|--------|------------------------------------------------------------------|
| `/`                    | GET    | Shell template + HTMX trigger for `/dashboard/data`              |
| `/dashboard/data`      | GET    | Renders the cached snapshot.  "Wait" partial when not yet primed |
| `/dashboard/refresh`   | GET    | Force live RPC fetch + cache write + render (Reload button)      |

### Displayed Information

**World clocks (above the status row):**
- Stack of 5 live 24-hour digital clocks updated client-side every
  second via `Intl.DateTimeFormat`.
- Row 1 is **UTC** (locked, blue-highlighted).
- Rows 2–5 are user-configurable.  Click the ✎ button on any
  row to set a custom label + IANA timezone.  Choices persist in
  `localStorage` under `bdsweb.clocks.v1`; the **↺ reset** button
  in the card header restores defaults (New York / London /
  Tokyo / Sydney).
- **Collapsible**: clicking the card header collapses the stack to
  a single-row UTC readout; clicking again expands it.  State is
  persisted in `localStorage` under `bdsweb.clocks.v1.collapsed`
  so each browser remembers its preferred view.  Keyboard a11y
  via Space / Enter on the focused header.
- The card lives in the dashboard *shell* (outside the HTMX swap
  target), so the per-second timer keeps ticking even while the
  rest of the page auto-refreshes.

**Status row (4 cards):**
- Node ID — the unique identifier of the connected bdsnode instance
- Hostname — OS hostname of the node
- Uptime — time since the node process started
- Total Records — aggregate count across all shards

**Timeline & Queues (2-column):**
- Data Timeline — timestamps of the oldest and newest stored events
- Ingest Queues — current depths of the log, JSON-file, and syslog-file
  ingestion queues; shown in yellow when non-zero, green when empty

**Shard chart:**
- Bar chart (Chart.js) showing telemetry record count per shard
- Shard start timestamp on the X-axis

**Shard table:**
- One row per shard: start timestamp, primary record count, secondary
  record count

### JSON-RPC calls made

| Method | Purpose |
|--------|---------|
| `v2/status` | Node ID, hostname, uptime, queue depths |
| `v2/count` | Total record count |
| `v2/timeline` | Oldest / newest event timestamps |
| `v2/shards` | Per-shard record counts |

The four calls are fired concurrently by the background poller via
`tokio::try_join!`; on success the snapshot replaces the cached one.

### Configuration

```hjson
// bds.hjson — refresh tuning for the Dashboard page
dashboard_refresh_secs: 30   // background poll interval + HTMX trigger
```

Floor 1 s.  Lower values pin more bdsweb CPU to RPC fan-out but pick
up changes faster.

---

## 5b. Cluster

**Path:** `GET /cluster`

Read-only cluster-health page modelled on the Dashboard.  A
background poller calls `v2/cluster.peers` once per
`cluster_refresh_secs` and parks the response in
`state.cluster_cache`; the page renders from the cache so
navigation never blocks on the RPC.  Auto-refreshes via HTMX every
`cluster_refresh_secs`; a **Reload** button forces a live fetch
through `/cluster/refresh`.

Renders a friendly "cluster mode is disabled" panel when
`cluster.enabled = false` on the connected node.

Routes:

| Route               | Method | Purpose                                                      |
|---------------------|--------|--------------------------------------------------------------|
| `/cluster`          | GET    | Shell template + HTMX trigger for `/cluster/data`            |
| `/cluster/data`     | GET    | Renders cached snapshot.  "Wait" partial when not yet primed |
| `/cluster/refresh`  | GET    | Force live `v2/cluster.peers` fetch + cache write + render   |

### Displayed Information

**This-node card** (when cluster enabled):
- Node ID · bind URL · embedding model · uptime seconds · mode badge
  (full / partial / standalone)

**Stat tiles row:**
- Alive peers · Suspect peers · Dead peers · full-mode threshold ·
  replication factor

**Replication health card (Phase 5):**
- Hint backlog (yellow when non-zero) · tombstone total ·
  last hint-tick age + replay count · last anti-entropy tick age +
  pulled / tombstones-applied / pruned counts

**Peer table:**
- One row per known peer: state badge · short node id (full id on
  hover) · URL · last_seen + age · version · embedding model ·
  miss_count · hint count (yellow when non-zero)

### JSON-RPC calls made

| Method               | Purpose                                                       |
|----------------------|---------------------------------------------------------------|
| `v2/cluster.peers`   | Full peer table + replication stats; unauthenticated read     |

### Configuration

```hjson
// bds.hjson — refresh tuning for the Cluster page
cluster_refresh_secs: 10     // background poll interval + HTMX trigger
```

Floor 1 s.  Defaults to 10 s — faster than the Dashboard because
peer-state changes (gossip transitions, fan-out failures, hint
queue churn) are what operators want to observe in near-real-time.

---

## 5c. Performance

**Path:** `GET /perf` (under **Administration → Performance** in the
top nav)

Live view of the in-process latency registry surfaced by `v2/perf`
plus the slow-query ring from `v2/perf.slow_queries`.  Diagnostic
page — no background poller; the data fetch runs every time the
page or its data partial is requested.  Auto-refreshes via HTMX
every `dashboard_refresh_secs` (same cadence as the Dashboard's
underlying `v2/status` poll), and the **Reload** button forces a
live fetch through `/perf/data`.

Routes:

| Route            | Method | Purpose                                                                  |
|------------------|--------|--------------------------------------------------------------------------|
| `/perf`          | GET    | Shell template + HTMX trigger for `/perf/data`                           |
| `/perf/data`     | GET    | Renders `v2/perf` + `v2/perf.slow_queries` in two tables                 |
| `/perf/analyze`  | GET    | "Analyze this!" — one-shot LLM analysis of the current snapshot           |

### Displayed Information

**Slow-query log card:**
- Threshold echoed (from `perf.slow_query_threshold_ms`, default 500 ms)
- Ring capacity (fixed at 100)
- Each entry: timestamp · series name · elapsed ms (yellow ≥ threshold,
  red ≥ 1000 ms)
- Empty-state message + "Slow log disabled" hint when threshold is 0

**Series table:**
- One row per active series, sorted by p95 descending
- Columns: name · n_total · n_recent · min · p50 · p95 · p99 · max (all µs)
- p95 cell tints yellow ≥ 100 ms, red ≥ 500 ms; p99 tints red ≥ 1 s

### Dashboard tile

The main Dashboard (`/`) carries a compact 4-card row driven by the
`perf` block of `v2/status` (no extra RPC).  Cards: ingest flush
p50/p95/p99 + lifetime count · ingest lag p95 · fan-out p95 max
(slowest v3/* method) · replicate p95 max (slowest write
replication).  The card row is hidden until the node has recorded
at least one ingest flush, so freshly-started nodes don't render
a wall of zeros.

### Analyze this!

The header on `/perf` carries an **Analyze this!** button next to
**Reload**, modelled on the Telemetry → Logs page.  Clicking it
fetches `v2/perf` + `v2/perf.slow_queries`, normalises the two
blocks into a single typed `rows` array (each entry carries a
`kind` discriminator — `"series"` or `"slow_entry"`), and hands the
payload to `v4/llm.analyze` with a perf-focused prompt template.
The result renders in a slide-out pane on the left side (mirror of
the Logs analysis pane); Esc dismisses it.

The default prompt teaches the model bdslib's series taxonomy —
`ingest.*` (DuckDB + Tantivy + HNSW + ONNX inside `add_batch`),
`shard.*` (per-shard search), `embed.*` (ONNX cache),
`fanout.*` (read fan-out RTT), `replicate.*` (write replication
RTT) — and asks for a six-point report covering headline verdict,
hot-path attribution, cluster shape, cache effectiveness, slow-log
outliers, and a single concrete next step citing a `bds.hjson`
knob.

Same operator semantics as the other "Analyze this!" surfaces:
one-shot (no chat history), `v4/llm.analyze` runs through the
configured default provider/model, response is rendered as
Markdown, identical inputs hit the inference cache so repeats are
free.

### JSON-RPC calls made

| Method                   | Purpose                                                |
|--------------------------|--------------------------------------------------------|
| `v2/perf`                | Full series snapshot                                   |
| `v2/perf.slow_queries`   | Recent slow events (ring buffer)                       |
| `v4/llm.analyze`         | One-shot perf analysis (HMAC-signed via `signed_rpc`)  |
| `v4/llm.providers.list`  | Resolve default provider/model for the wait banner     |
| `v2/status` (Dashboard)  | Carries the compact `perf` block consumed by the tile  |

### Configuration

The page inherits `dashboard_refresh_secs` for its HTMX auto-refresh
cadence.  Threshold tuning lives in the bdsnode side
(`perf.slow_query_threshold_ms` — see BDSCONFIG.md §4.2).

The "Analyze this!" surface is tunable via the standard
`web.analyze.<target>` block — for this page, **`web.analyze.perf`**:

```hjson
web: {
  analyze: {
    perf: {
      timeout_secs:    600
      max_rows:        200
      // Optional: replace the baked perf-focused prompt entirely.
      // prompt_template: '''…'''
    }
  }
}
```

| Field                            | Type    | Default | Description                                                                              |
|----------------------------------|---------|---------|------------------------------------------------------------------------------------------|
| `web.analyze.perf.timeout_secs`  | integer | 600     | Per-call deadline for `v4/llm.analyze`.  Floor 30.                                       |
| `web.analyze.perf.max_rows`      | integer | 200     | Cap on combined series + slow-log entries sent to the LLM.  Series sorted by p95 desc; slow entries newest-first; truncation drops the tail. |
| `web.analyze.perf.prompt_template` | string | baked   | Optional override of the default sharded-mixed-engine prompt.                            |

---

## 6. Telemetry Search

**Path:** `GET /telemetry`

Semantic (vector) search over stored telemetry records.

### Query Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `q` | string | `""` | Natural-language search query |
| `duration` | string | `1h` | Look-back window (see Duration Values) |

### Duration Values

`15min`, `30min`, `1h`, `2h`, `4h`, `6h`, `12h`, `24h`, `7days`

### Interactions

- **Typing in the query field** triggers a search automatically after a
  450 ms debounce.
- **Changing the duration** reloads the key cloud immediately.
- **Submit button** runs the search explicitly.

### Displayed Information

**Key cloud** — clickable tag buttons for every known key in the selected
duration. Clicking a key sets it as the search query.

**Results table** — one row per matching record:
- Timestamp
- Key (metric or event name)
- Data (truncated to 120 characters)
- Score (cosine similarity, 3 decimal places)

### JSON-RPC calls made

| Method | Trigger |
|--------|---------|
| `v2/keys.all` | Page load; duration change |
| `v2/search.get` | Query submit / debounced input |

---

## 7. Log Search

**Path:** `GET /logs`

Semantic search over stored log entries, with an LDA topic sidebar and a
floating results panel.

### Query Parameters

Same as Telemetry: `q` and `duration`.

### Interactions

- **Typing** in the query field (450 ms debounce) opens the results panel.
- **Changing duration** reloads both the key cloud and the topic cloud.
- **Esc key** or the **✕ button** closes the results panel.
- **Clicking a topic keyword** sets it as the search query.

### Displayed Information

**Left sidebar — Known Keys:** clickable tag cloud of log source keys for
the selected duration.

**Right sidebar — Topic Keywords:** LDA-derived topics, each with several
representative keywords. Keywords are clickable and link to a search query.

**Floating results panel** (slides in from the right, 660 px wide):
- Result count and current query displayed in the panel header
- Table: Timestamp, Key, Message, Score
- Long JSON messages wrap within the panel (word-break enforced)
- Panel persists until explicitly closed

### JSON-RPC calls made

| Method | Trigger |
|--------|---------|
| `v2/keys.all` | Page load; duration change |
| `v2/topics.all` | Page load; duration change |
| `v2/search.get` | Query submit / debounced input |

---

## 8. Document Search

**Path:** `GET /docs`

Semantic search over stored documents (runbooks, tickets, post-mortems,
knowledge-base articles).

### Query Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `q` | string | `""` | Natural-language search query |
| `limit` | integer | `10` | Maximum results: 5, 10, 20, 50 |

### Interactions

- **Typing** in the query field (500 ms debounce) runs the search.
- **Changing the limit** re-runs the current query immediately.

### Displayed Information

One **card per document**:
- **Name** — from `metadata.name` or `metadata.document_name`
- **Category badge** — colour-coded: runbook (blue), ticket (yellow),
  postmortem (red), kb (purple), change (green)
- **Document ID** (UUID)
- **Preview** — first 280 characters of content
- **Score** — similarity score in green
- **Expandable metadata** — full metadata JSON in a `<details>` block

### JSON-RPC calls made

| Method | Trigger |
|--------|---------|
| `v2/doc.search` | Query submit / debounced input |

---

## 9. Aggregated Search

**Path:** `GET /search`

Runs a single query simultaneously against both telemetry records and
documents, displaying results side by side.

### Query Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `q` | string | `""` | Natural-language search query |
| `duration` | string | `1h` | Look-back window for telemetry hits |

### Displayed Information

Two-column layout (stacks on narrow viewports):

**Observability (left):** matching telemetry records — Timestamp, Key,
Data, Score — with a hit-count badge.

**Documents (right):** matching documents — Name, Category, Score,
Preview — with a hit-count badge.

### JSON-RPC calls made

| Method | Trigger |
|--------|---------|
| `v2/aggregationsearch` | Query submit |

---

## 10. Trends

**Path:** `GET /trends`

Statistical analysis and time-series visualisation for a single metric key.

### Query Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `key` | string | `""` | Exact metric key (e.g. `cpu.usage`, `http.latency_ms`) |
| `duration` | string | `1h` | Look-back window |

### Interactions

- Enter a metric key and click **Analyse** (or press Enter).
- Changing the duration re-runs the analysis.

### Displayed Information

**Statistics grid (4 cards):** sample count (n), mean, standard deviation,
variability coefficient.

**Statistics row (3 cards):** minimum, median, maximum.

**Alert badges** (when applicable): anomaly count, breakout count.

**Time-series chart** (uPlot):
- X-axis: wall-clock time
- Y-axis: metric value (auto-scaled)
- Blue line with light-blue fill
- Data points shown as dots when ≤ 200 samples

### JSON-RPC calls made

| Method | Purpose |
|--------|---------|
| `v2/trends` | Statistical summary and anomaly detection |
| `v2/primaries.get.telemetry` | Raw time-series data points for the chart |

---

## 11. Bund Workbench

**Path:** `GET /bund`

An interactive workbench for the BUND stack-based scripting language.
Scripts are evaluated against a named VM context that persists state between
runs.

### Using the Editor

1. Type a BUND script in the CodeMirror editor.
2. Optionally change the **Context** name (default: `default`). The same
   context name reuses accumulated VM state across runs; enter a new name or
   click **↺** to start with a fresh context.
3. Press **Run** (or **⌘↵** / **Ctrl+↵**) to evaluate.
4. The result is the last value pushed to the workbench (`vm.stack.workbench`)
   by the script.

### Syntax Highlighting

The editor provides full BUND language colouring:

| Colour | Token type | Examples |
|--------|-----------|---------|
| Blue, bold | Keywords | `if`, `while`, `for`, `map`, `register`, `alias` |
| Red | Builtins | `dup`, `drop`, `push`, `type`, `string.upper`, `float.sqrt` |
| Green | Double-quoted strings | `"hello world"` |
| Emerald | Single-quoted literals | `'symbol'` |
| Pink | Atoms | `:ok`, `:error` |
| Violet | Pointers | `` `myword `` |
| Cyan | Named stacks | `@context` |
| Orange | Numbers | `42`, `3.14`, `-1e5` |
| Amber | Brackets | `{`, `}`, `[`, `]` |
| Teal | Operators | `+`, `-`, `*`, `/`, `>=`, `==` |
| Grey | Comments | `// comment` |

### Output

| Condition | Display |
|-----------|---------|
| Script pushed a value with `.` | Pretty-printed JSON of the last workbench value |
| Script ran without pushing to workbench | "Script ran — workbench is empty." |
| RPC or evaluation error | Red error box with the error message |

### Context Management

| Action | Effect |
|--------|--------|
| Same context name across runs | VM state (defined words, stack) accumulates |
| Different context name | Fresh VM with only stdlib loaded |
| Click **↺** | Generates a random context name (`ctx-XXXXXXXX`) |

Contexts are evicted server-side after a configurable idle timeout
(default 300 s, set via `bund_ttl_secs` in the bdsnode config).

### Example Scripts

```bund
// Arithmetic — push result to workbench
2 2 + .

// String operation
"hello" string.upper .

// List processing
[ 1 2 3 4 5 ] dup len swap
```

### Translate from English (`v2/to.bund`)

A collapsible **Translate from English** card sits above the
CodeMirror editor.  Click to expand, type an English request, click
**Translate** — bdsweb posts the message to `/bund/translate` (which
forwards to [`v2/to.bund`](../jsonrpc_api/v2_to_bund.md)) and renders
the result inline:

| Element                | Source                                  |
|------------------------|-----------------------------------------|
| Validity badge         | `valid` — green ✓ or amber ⚠            |
| Attempt counter        | `parse_attempts` (1 = first-try success)|
| Provider / model       | `provider` + `model`                    |
| Wall-clock cost        | `ms`                                    |
| Script body            | `script` (scrollable `<pre>`)           |
| "Use as script" button | Drops `script` into the CodeMirror editor below |
| "last validation error"| Collapsible `<details>` with `parse_error`; only present when `valid=false` |

Workflow: **English → Translate → review → Use as script → Run**.
The CodeMirror editor stays untouched until you click **Use as
script**, so you can experiment with different prompts without
losing whatever's already in the editor.

The card is closed by default — no extra render cost on page load.
When the v2/to.bund endpoint is disabled (`llm.to_bund.enabled =
false`) or the cluster has no LLM provider registered, the panel
renders the RPC error inline (red bar) instead of a script.

See [`BDSCMD.md`](BDSCMD.md) § _to-bund_ for the same surface in
shell form, and [`LLM.md`](LLM.md) § _English → Bund translator_
for the prompt assembly + retry semantics.

### JSON-RPC calls made

| Method | Trigger |
|--------|---------|
| `v2/eval` | Run button / ⌘↵ |
| `v2/to.bund` | Translate-from-English **Translate** button |

---

## 11b. Help pane

**Path:** `GET /help`

Natural-language Q&A over the cluster docstore.  The page hosts a
single text field, a checkbox, an optional limit input, and a
**Help!** button; submitting the form posts to `/help/query`, which
calls [`v3/help`](../jsonrpc_api/v3_help.md) and renders the LLM
answer inline with full markdown formatting.

### Controls

| Control                  | Purpose                                                                                  |
|--------------------------|------------------------------------------------------------------------------------------|
| **Question** (textarea)  | The English question.  `⌘↵` / `Ctrl↵` from inside the textarea submits.                  |
| **Internal docs only**   | Sends `internal_only=true` — restricts retrieval to documents tagged `metadata.internal_doc == true` (the corpus loaded by `scripts/load_internal_documentation.sh`). |
| **Limit** (number input) | Number of documents to feed into the prompt.  Range `1..50`; default `8` (server-side).  |
| **Help!** button         | Submits the form.                                                                        |

### Result pane

The partial returned by `POST /help/query` carries:

| Element              | Source                                                                              |
|----------------------|-------------------------------------------------------------------------------------|
| Validity badge       | Green `✓ N docs` when `n_docs > 0`, amber `⚠ 0 docs matched` otherwise.            |
| `internal only` pill | Shown only when the request was issued with `internal_only=true`.                   |
| Provider / model     | Echoed from the response.                                                           |
| Limit / ms / tokens  | `limit·{ms}ms·{tokens_in}→{tokens_out} tok`.  Tokens row hidden when none reported. |
| Empty-corpus note    | Shown only when the server set the `note` field (no documents matched).             |
| Answer body          | Rendered via `crate::markdown::render` — full GitHub-style Markdown including `<h1>`–`<h4>`, tables, fenced code, blockquotes, task lists.  Sanitised through ammonia's allowlist before interpolation. |
| Sources              | One pill per cited document — score + name; pills with `internal_doc=true` get a distinct blue border. |

When `v3/help` returns an RPC error (e.g. provider unreachable,
docstore search failure) the pane renders a red error bar with the
upstream message instead of an answer.

### Routes

| Path           | Method | Notes                                                                |
|----------------|--------|----------------------------------------------------------------------|
| `/help`        | GET    | Empty page with the search form.                                     |
| `/help/query`  | POST   | HTMX target; renders `partials/help_result.html` with the response.  |

### JSON-RPC calls made

| Method     | Trigger          |
|------------|------------------|
| `v3/help`  | Help! button     |

### Timeout

`/help/query` uses a 5-minute reqwest timeout (matches `/chat`).
This accommodates slow local Ollama deployments running CPU-bound
inference over a fat prompt; cloud providers normally answer in
under 10 seconds.

---

## 12. Common UI Patterns

### Debounced Search

Telemetry, Logs, and Document search fields fire automatically after the
user stops typing (450–500 ms delay), avoiding excessive requests during
fast input.

### HTMX Partial Updates

All search results and dynamic sections use HTMX to replace only the
relevant DOM fragment. The rest of the page is not reloaded. An inline
spinner (e.g. "Searching…") appears during in-flight requests.

### Duration Selector

All search pages share the same look-back window options:
`15min` · `30min` · `1h` · `2h` · `4h` · `6h` · `12h` · `24h` · `7days`

The selected value is preserved in the URL query string so pages can be
bookmarked or shared.

### Error Display

Handler-level RPC errors that a page knows how to absorb (an empty
result set, an upstream warning) are rendered inline as a red
bordered box — no full-page reload.

Hard failures (bdsnode unreachable, a handler panic) produce an
HTTP 500.  Three layers shape what the browser actually sees:

- **Generic client message.** The 500 page never echoes the raw
  error — it can name the backend node URL or internal response
  text.  The full detail is logged server-side; the visitor sees a
  class-appropriate line ("Couldn't reach the bdsnode backend…",
  "The bdsnode backend returned an error", …).
- **HTMX-aware fragments.** When the failing request was an HTMX
  partial swap, a middleware replaces the full `<!doctype html>`
  error document with a compact inline fragment ("This section
  couldn't load — Reload page" button) so a transient blip doesn't
  inject a whole HTML page into a results panel.
- **Panic boundary.** A `CatchPanicLayer` wraps the entire router,
  so a panic in any handler or middleware becomes a clean 500
  instead of a dropped connection.

See [§ 15](#15-resilience--health) for the retry and
graceful-degradation behaviour behind these.

### Frontend Libraries

| Library | Version | Use |
|---------|---------|-----|
| Tailwind CSS | CDN | Layout and styling (dark theme) |
| HTMX | 2.0.2 | Partial page updates |
| Chart.js | 4.4.4 | Dashboard shard bar chart |
| uPlot | 1.6.31 | Trends time-series chart |
| CodeMirror | 5.65.16 | Bund editor with syntax highlighting |

---

## 13. Authentication

### Modes

bdsweb runs in one of two modes determined at startup by what's in
the config file passed via `--config` / `BDS_CONFIG`:

| Mode | Trigger | Behaviour |
|---|---|---|
| **Open-access** | No `--config`, or config has no `cluster` block / `cluster.enabled = false` | Auth middleware is a no-op.  Every route is wide open.  The `/login` page renders a yellow banner explaining why no challenge is presented.  **bdsweb refuses to start in this mode on a non-loopback `--host`** — see the open-access bind guard in [§ 2](#2-global-options).  Use only on a loopback dev box. |
| **Authenticated** | Config has `cluster.enabled = true` AND `cluster.shared_secret` populated | Every route except the public allow-list (`/login`, `/logout`, `/version`, `/whoami`, `/healthz`) requires a valid `bds_session` cookie.  Unauthenticated requests redirect to `/login?next=<original-path>`. |

The bdsweb startup banner says which mode it ended up in:

```
[INFO] bdsweb auth enabled — shared_secret loaded (36 bytes)
# or — a loud multi-line banner:
[WARN] ============================================================
[WARN]   bdsweb is running in OPEN-ACCESS mode — SESSION AUTH IS OFF
[WARN]   No cluster.shared_secret configured. Every page, including
[WARN]   /admin/* and /bund (arbitrary Bund eval), is unauthenticated.
[WARN]   Bound to loopback (127.0.0.1) only — do not expose this process.
[WARN] ============================================================
```

### First-user bootstrap

When the cluster's user store is empty (probed via `v3/user.list`),
the middleware passes every request through unconditionally so an
operator can reach `/admin/users` to mint the first admin without a
chicken-and-egg login wall.  Two guards keep this window tight:

- **Loopback-only.** The bypass is granted only to loopback peer
  connections — it is never reachable from the network, even
  briefly.
- **Closed result cached, open result is not.** The "store is
  empty" verdict is *not* cached; bdsweb re-probes on every request
  while the window is open, so creating the first user closes auth
  on the very next request rather than up to 30 s later.  Only the
  "store is non-empty" verdict is cached (30 s).

### Routes

| Path | Method | Purpose |
|---|---|---|
| `/login`         | GET  | Render the login form. Accepts `?next=<path>` for post-login redirect — the value is validated as a same-origin absolute path (`//host`, `https://…`, scheme-relative URLs are rejected and fall back to `/`). |
| `/login`         | POST | Submit `username` + `password` → `v3/user.authenticate` → set `bds_session` cookie → 303 to the validated `next` (or `/`).  Per-IP rate-limited (see *Rate limiting* below). |
| `/logout`        | POST | Clear the cookie and 303 to `/login`. |
| `/admin/users`   | GET  | Render the User management table.  Read-only — issues `v3/user.list` HMAC-signed. |
| `/admin/users/add` | POST | Form-encoded `username, password, [display_name]` → `v3/user.add` (HMAC) → 303 with `notice=added` query. |
| `/admin/users/reset_password/{id}` | POST | Form-encoded `password` → `v3/user.modify` (HMAC) → 303 with `notice=password-reset`. |
| `/admin/users/disable/{id}` | POST | `v3/user.modify {disabled:true}` (HMAC) → 303 with `notice=disabled`. |
| `/admin/users/enable/{id}`  | POST | `v3/user.modify {disabled:false}` (HMAC) → 303 with `notice=enabled`. |
| `/admin/users/delete/{id}`  | POST | `v3/user.delete` (HMAC) → 303 with `notice=deleted`. |

### Cookie

`bds_session=<TOKEN>; HttpOnly; SameSite=Lax; Path=/; Max-Age=<session_ttl>[; Secure]`

The token is a stateless HMAC-signed payload (`<user_id>.<expires_at>.<hmac>`)
issued by `v3/user.authenticate` and verified offline on each request via
`bdslib::cluster::session::verify_session_token`.  No central session
store; deletion of the cookie is the only logout mechanism.  Set the
TTL (`cluster.session_ttl` in `bds.hjson`, default `8h`) according to
your threat model.

**`Secure` flag** — controlled by `web.secure_cookies` (see
[BDSCONFIG.md § 8.2](BDSCONFIG.md#82-web--security--tls)).  When the
key is absent it is resolved automatically: `Secure` is set when
`web.tls` is enabled **or** the bind host is non-loopback, and left
off only for a plain-HTTP loopback dev box.  Set it explicitly to
override the heuristic — e.g. `web.secure_cookies: true` when bdsweb
is HTTP-only on loopback but a TLS-terminating reverse proxy fronts
it.

### CSRF protection

State-changing requests are protected without a per-form token:

- **No side-effecting GETs.**  Every endpoint that mutates state or
  triggers work — `/chat/reset`, all `/*/analyze` LLM calls, every
  admin action — is `POST` (or `DELETE`).  A cross-site `<img src>`
  or top-level navigation therefore cannot trigger them.
- **Same-origin enforcement.**  A middleware rejects any
  `POST`/`PUT`/`PATCH`/`DELETE` whose `Origin` (or, as a fallback,
  `Referer`) host does not match the request `Host`, with a `403`.
  A cross-site form auto-submit or `fetch()` always carries the
  attacker's origin, so it is blocked.
- The post-login `?next=` redirect target is validated as a
  same-origin absolute path, closing the open-redirect vector.

### Security response headers

Every response carries: `X-Frame-Options: DENY`,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: same-origin`,
and `Content-Security-Policy: frame-ancestors 'none'` — clickjacking
and MIME-sniffing protection across the whole surface, including
`/admin/*`.

### Rate limiting

`POST /login` is rate-limited per client IP
(`cluster.auth_rate_limit_per_minute`, default 10/min; `0` disables).
By default the limiter keys on the TCP peer IP.  When bdsweb sits
behind a reverse proxy, set `web.trusted_proxy: true` so it keys on
the forwarded client IP (`X-Forwarded-For` / `X-Real-IP`) instead —
see [BDSCONFIG.md § 8.2](BDSCONFIG.md#82-web--security--tls).  Leave
it `false` otherwise: the header is spoofable by a direct client.

### Administration → User management page

Located at `/admin/users`.  Accessible to every authenticated user
(no RBAC in v1 — every logged-in user can manage every user).  The
page consists of:

1. **Add-user form** at the top with `username`, `password`,
   optional `display name` fields.
2. **User table** listing all users with columns: username, display
   name, auth method, created/updated timestamps, status badge
   (active / disabled), and a per-row action menu (⋯).

   The action menu opens an inline panel with:
   - **Reset password** — small password input + Reset button.
     Submits to `/admin/users/reset_password/<id>`.
   - **Disable / Re-enable** — toggle.
   - **Delete** — with `confirm()` dialog warning that the delete
     fans out to every peer.

All mutations issue a 303 redirect back to `/admin/users?notice=…`
or `?error=…` so the user sees a green or red banner on the next
page load.

### Where the navigation lives

The `Administration` dropdown is the **rightmost** item in the main
top nav (`margin-left: auto`).  It contains a single sub-link to
`/admin/users` ("User management").  Future admin pages
(replication health, audit log, RBAC) hang off the same dropdown.

### JSON-RPC calls behind the auth surface

| User action | RPC method | HMAC |
|---|---|---|
| POST `/login` | `v3/user.authenticate` | no |
| `/admin/users` page load | `v3/user.list` | yes |
| Add user form submit | `v3/user.add` | yes (or unsigned during bootstrap) |
| Reset password / disable / enable | `v3/user.modify` | yes |
| Delete user | `v3/user.delete` | yes |

---

## 14. LLM — Chat + Administration

Two pages consume the `v4/llm.*` surface (see [`LLM.md`](LLM.md) for
the full architecture).  Every call goes through `admin::signed_rpc`
— v4/* refuses unsigned requests, so the LLM features only work when
`cluster.shared_secret` is configured.  Open-access mode (no secret)
shows banners on both pages explaining the situation.

### /chat — provider-aware RAG chat

`GET /chat` renders the chat session UI; `POST /chat/new` opens a
fresh session with a key-inventory briefing; `POST /chat/query`
sends a follow-up turn; `GET /chat/reset` clears the
`bds-chat-session` cookie (keeps the provider preference).

Per-turn controls (above the message textarea):

| Control       | Form field | Notes                                                          |
|---------------|-----------|----------------------------------------------------------------|
| **Provider**  | `provider`| Dropdown populated on page load from `v4/llm.providers.list`. Labelled `<id> (<default_model>)`.  Sticky via the `bds-chat-provider` HttpOnly cookie. |
| **Context window** | `duration` | `15m` / `30m` / `1h` (default) / `3h` / `6h` / `12h` / `1day`.  Used as the RAG lookback. |
| **Your question**  | `query`    | The actual message.  Ctrl/Cmd+Enter submits. |

Header banner above each assistant reply shows what the model
actually saw:

```
208 telemetry events + 11 documents · last 1h · prompt=14823ch · num_ctx=32768
· provider=ollama model=llama3.2
```

`prompt=…ch` is the assembled prompt length; `num_ctx=…` is the
Ollama context window auto-sized from prompt size to prevent silent
truncation (Ollama defaults to 2048 tokens; see [`LLM.md`](LLM.md)
§ _Operational gotchas_).

When both `telemetry_count` and `document_count` are 0 the banner
flips to a yellow warning:

```
⚠ NO RAG context loaded for last 1h — model is answering without
your data · provider=ollama model=llama3.2
```

so the operator immediately knows why a response looks hallucinated.

### /admin/llm — providers + cache + jobs

Cards in order:

- **Providers** — table from `v4/llm.providers.list`: id /
  default_model / chat / embed / ★ on the configured default.
  When no providers are registered, a helpful "Add an
  `llm.providers.*` block to bds.hjson and restart bdsnode" line
  replaces the table.

- **Inference cache** — `v4/llm.cache.stats` totals (rows / total
  hits / response bytes / TTL flag).  Inline purge form below the
  stats with three filters (provider, kind, older-than-secs); empty
  filter set purges EVERYTHING with a JS `confirm()` guard.

- **Recent async jobs** — `v4/llm.jobs.list?limit=20` with
  state-coloured rows: done=emerald · failed=red · cancelled=amber
  · running=sky · pending=slate.  Each row shows truncated
  `job_id`, kind, state, submitted/finished timestamps, and any
  error message.

### Where the navigation lives

The `Administration` dropdown (rightmost, `margin-left: auto`) now
contains two sub-links:

- **User management** → `/admin/users`
- **LLM** → `/admin/llm`

### JSON-RPC calls behind the LLM surface

| User action                          | RPC method                                | HMAC |
|--------------------------------------|-------------------------------------------|------|
| `/chat` page load                    | `v4/llm.providers.list`                   | yes  |
| `/chat/new` form submit              | `v2/primaries.explore` then `v4/llm.chat` | last only |
| `/chat/query` form submit            | `v4/llm.chat`                             | yes  |
| `/admin/llm` page load               | `v4/llm.providers.list` + `v4/llm.cache.stats` + `v4/llm.jobs.list` | yes |
| `/admin/llm/purge` form submit       | `v4/llm.cache.purge`                      | yes  |

---

## 15. Resilience & health

bdsweb is built to stay useful while bdsnode is slow, restarting, or
briefly unreachable — and to make that state visible.

### Graceful degradation

- **Cached snapshots.**  The Dashboard and Cluster pages render from
  background-poller caches; a failed poll keeps the previous
  snapshot rather than blanking the page.
- **Partial dashboard.**  The Dashboard poll fans out four RPCs with
  `join!` (not `try_join!`).  If only some fail, the surviving
  sections render fresh and the failed ones carry their last-known
  value forward, marked by an amber "Showing last-known data for …"
  banner — one slow `v2/shards` no longer blanks the whole page.
- **"Backend unreachable" escalation.**  If the cache is still empty
  ~30 s after startup, the Dashboard / Cluster "Wait…" partial
  switches to an amber *"bdsnode backend appears unreachable — still
  retrying"* state instead of spinning silently forever.

### Retry & supervision

- **Bounded RPC retry.**  Transient `reqwest` failures (connection
  refused, timeout — bdsnode restarting, a TCP blip) are retried up
  to 3 times with a short backoff.  RPC-level errors are
  deterministic and are *not* retried.
- **Supervised pollers.**  The Dashboard and Cluster background
  pollers run under a supervisor: if a poll task ever panics or
  exits, it is logged and re-spawned after a short delay — a poller
  never dies silently and freezes its cache.
- **Panic boundary.**  `CatchPanicLayer` turns any handler /
  middleware panic into a clean 500 (see [§ 12](#12-common-ui-patterns)).

### `/healthz` — readiness probe

| Path | Method | Auth | Purpose |
|---|---|---|---|
| `/healthz` | GET | public (allow-listed) | Backend-connectivity readiness verdict |

Returns JSON derived purely from the dashboard poller's last
success — no extra RPC:

```json
{
  "status": "ok",
  "bdsnode": { "last_ok_unix": 1778773114, "age_secs": 12, "poll_interval_secs": 30 },
  "ts": 1778773126
}
```

| `status` | HTTP | Meaning |
|---|---|---|
| `starting` | 200 | The poller has not completed a first successful fetch yet. |
| `ok` | 200 | Last successful poll is within ~3 poll intervals. |
| `degraded` | 503 | Last success is older than that — bdsnode looks unreachable. |

The `503` on `degraded` lets a load balancer drain a bdsweb whose
backend is down, while `bdsweb` itself stays up serving cached
pages and `/login`.

---

## 16. TLS / HTTPS

bdsweb can serve HTTPS directly — no reverse proxy required — via the
optional `web.tls` block in `bds.hjson`:

```hjson
web: {
  tls: {
    enabled: true
    cert: "/etc/bdsweb/tls/cert.pem"
    key:  "/etc/bdsweb/tls/key.pem"
  }
}
```

When enabled, `cert` and `key` are **required**; an incomplete block
makes bdsweb refuse to start (it never silently downgrades to HTTP),
and a bad/unreadable cert or key is a hard fail-fast startup error.
Enabling TLS also flips the `web.secure_cookies` auto-default to
`true`.

bdsnode binaries embed the `ring` crypto provider, and the listener
speaks TLS 1.2/1.3.  The startup banner shows the scheme:

```
[INFO] bdsweb listening on https://127.0.0.1:8080  →  bdsnode at http://127.0.0.1:9000
```

A full zero-to-running walkthrough — generating a development
certificate, obtaining a real one, wiring the config, verifying the
handshake, and the reverse-proxy alternative — is in
[`BDSWEB_TLS.md`](BDSWEB_TLS.md).
