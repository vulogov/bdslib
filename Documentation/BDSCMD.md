# bdscmd — bdsnode JSON-RPC Client

`bdscmd` is a full-featured command-line client for every method exposed by
`bdsnode`'s JSON-RPC 2.0 API. Each API method has its own subcommand.
Results are printed as pretty-printed JSON; pass `--raw` for compact single-line
output suitable for piping into `jq`.

---

## Table of Contents

1. [Installation](#1-installation)
2. [Global Options](#2-global-options)
3. [Environment Variables](#3-environment-variables)
4. [Server Pre-flight Check](#4-server-pre-flight-check)
5. [Output Format](#5-output-format)
6. [Commands — Ingestion](#6-commands--ingestion)
   - [status](#61-status)
   - [add](#62-add)
   - [add-batch](#63-add-batch)
   - [add-file](#64-add-file)
   - [add-file-syslog](#65-add-file-syslog)
7. [Commands — Inventory](#7-commands--inventory)
   - [timeline](#71-timeline)
   - [count](#72-count)
   - [shards](#73-shards)
8. [Commands — Keys](#8-commands--keys)
   - [keys](#81-keys)
   - [keys-all](#82-keys-all)
   - [keys-get](#83-keys-get)
9. [Commands — Primaries & Secondaries](#9-commands--primaries--secondaries)
   - [primaries](#91-primaries)
   - [primaries-explore](#92-primaries-explore)
   - [primaries-explore-telemetry](#93-primaries-explore-telemetry)
   - [primaries-get](#94-primaries-get)
   - [primaries-get-telemetry](#95-primaries-get-telemetry)
   - [primary](#96-primary)
   - [secondaries](#97-secondaries)
   - [secondary](#98-secondary)
   - [duplicates](#99-duplicates)
10. [Commands — Search](#10-commands--search)
    - [fulltext](#101-fulltext)
    - [fulltext-get](#102-fulltext-get)
    - [fulltext-recent](#103-fulltext-recent)
    - [search](#104-search)
    - [search-get](#105-search-get)
11. [Commands — Analysis](#11-commands--analysis)
    - [trends](#111-trends)
    - [topics](#112-topics)
    - [topics-all](#113-topics-all)
    - [rca](#114-rca)
    - [rca-templates](#115-rca-templates)
12. [Commands — Template Store](#12-commands--template-store)
    - [tpl-add](#121-tpl-add)
    - [tpl-get](#122-tpl-get)
    - [tpl-delete](#123-tpl-delete)
    - [tpl-list](#124-tpl-list)
    - [tpl-search](#125-tpl-search)
    - [tpl-update](#126-tpl-update)
    - [tpl-reindex](#127-tpl-reindex)
    - [tpl-template-by-id](#128-tpl-template-by-id)
    - [tpl-templates-by-timestamp](#129-tpl-templates-by-timestamp)
    - [tpl-templates-recent](#1210-tpl-templates-recent)
13. [Commands — BUND VM](#13-commands--bund-vm)
    - [eval](#131-eval)
    - [Shebang scripts](#132-shebang-scripts)
14. [Quick Reference](#14-quick-reference)
15. [Exit Codes](#15-exit-codes)

---

## 1. Installation

`bdscmd` is built from the same Cargo workspace as `bdslib` and `bdsnode`. No
additional dependencies are required.

```bash
# development build
cargo build --bin bdscmd

# release build
cargo build --release --bin bdscmd
cp target/release/bdscmd /usr/local/bin/
```

---

## 2. Global Options

All flags must appear **before** the subcommand name.

| Flag | Short | Env var | Default | Description |
|---|---|---|---|---|
| `--address <ADDR>` | `-a` | `BDSCMD_ADDR` | `http://127.0.0.1:9000` | bdsnode address: bare `host:port` or full URL |
| `--session <UUID>` | `-s` | `BDSCMD_SESSION` | auto-generated UUID v7 | Session identifier included in every request |
| `--raw` | `-r` | — | false | Print compact (non-pretty) JSON |
| `--help` | `-h` | — | — | Print help and exit |

```bash
# explicit address
bdscmd -a 10.0.0.5:9000 status

# address as full URL
bdscmd -a http://bdsnode.internal:9944 count -d 1h

# suppress pretty-printing
bdscmd --raw status | jq '.node_id'
```

---

## 3. Environment Variables

| Variable | Description |
|---|---|
| `BDSCMD_ADDR` | Default server address (overridden by `--address`) |
| `BDSCMD_SESSION` | Default session UUID (overridden by `--session`) |

```bash
export BDSCMD_ADDR=http://bdsnode.prod:9000
export BDSCMD_SESSION=my-fixed-session-id

# all subsequent calls use the exported values
bdscmd status
bdscmd count -d 1h
```

---

## 4. Server Pre-flight Check

Every subcommand except `status` calls `v2/status` before sending its own
request. If the server is not reachable the command fails immediately with a
clear error, preventing silent data loss or misleading timeouts.

```
error: server pre-flight check failed for http://127.0.0.1:9000

Caused by:
    bdsnode not reachable at http://127.0.0.1:9000
```

`status` itself is exempt so that `bdscmd status` can be used as a health probe
even while the node is starting up.

---

## 5. Output Format

Every subcommand prints the `result` field of the JSON-RPC response to stdout as
pretty-printed JSON. Errors are written to stderr and cause a non-zero exit code.

```bash
# pretty-printed (default)
bdscmd count -d 1h
# {
#   "count": 8471
# }

# compact, pipe-friendly
bdscmd --raw count -d 1h | jq '.count'
# 8471
```

---

## 6. Commands — Ingestion

### 6.1 `status`

Show a live process snapshot of the running `bdsnode`. This is the only command
that does **not** perform a pre-flight server check.

```
bdscmd status
```

**Example output:**

```json
{
  "node_id": "019735e2-7c1a-7000-85fd-c17a3b8f912a",
  "hostname": "prod-node-01",
  "uptime_secs": 3612,
  "started_at": 1745600000,
  "queue_depth": 0,
  "file_queue": 0,
  "file_name": null,
  "syslog_file_queue": 0,
  "syslog_file_name": null
}
```

Use `status` to check whether `bdsnode` is up and to monitor background file
ingestion progress:

```bash
# wait until both file queues drain
while true; do
  bdscmd --raw status | jq '{fq: .file_queue, sfq: .syslog_file_queue}'
  sleep 2
done
```

---

### 6.2 `add`

Ingest a single telemetry document. The document may be passed as an inline JSON
string or read from stdin.

```
bdscmd add [DOC]
```

| Argument | Description |
|---|---|
| `DOC` | JSON object. Omit or pass `-` to read from stdin. |

The document must be a valid telemetry record with at least `key` and `data`
fields (see `v2/add` API docs for the full schema).

**Examples:**

```bash
# inline document
bdscmd add '{"key":"cpu.usage","data":0.73,"timestamp":1745600000}'

# from stdin
echo '{"key":"mem.used","data":4294967296}' | bdscmd add

# heredoc
bdscmd add <<'EOF'
{
  "key": "disk.io",
  "data": {"read_mbps": 120, "write_mbps": 45},
  "host": "web-01"
}
EOF
```

**Output:**

```json
{
  "queued": 1
}
```

---

### 6.3 `add-batch`

Ingest multiple documents in a single request. Accepts either a JSON array or an
NDJSON stream (one document per line).

```
bdscmd add-batch [SOURCE]
```

| Argument | Description |
|---|---|
| `SOURCE` | Path to a JSON array or NDJSON file, or `-` / omitted for stdin |

**Examples:**

```bash
# JSON array from stdin
printf '[{"key":"a","data":1},{"key":"b","data":2}]' | bdscmd add-batch

# NDJSON file
bdscmd add-batch /tmp/events.ndjson

# NDJSON from another tool
bdscli generate log -n 500 | bdscmd add-batch
```

**Output:**

```json
{
  "queued": 500
}
```

---

### 6.4 `add-file`

Queue an NDJSON file for background ingestion. The path must be accessible from
the server's filesystem (not the client's).

```
bdscmd add-file <PATH>
```

| Argument | Description |
|---|---|
| `PATH` | Absolute path to the NDJSON file on the server's filesystem |

The file is validated (exists, is a regular file, non-empty, readable) before
being queued. Use `bdscmd status` to monitor the `file_queue` and `file_name`
fields until ingestion completes.

**Example:**

```bash
bdscmd add-file /data/logs/events-2026-04-26.ndjson

# monitor until done
while [[ "$(bdscmd --raw status | jq '.file_queue')" -gt 0 ]]; do
  sleep 1
done
echo "ingestion complete"
```

**Output:**

```json
{
  "queued": "/data/logs/events-2026-04-26.ndjson"
}
```

---

### 6.5 `add-file-syslog`

Queue an RFC 3164 syslog file for background ingestion. Each syslog line is
parsed and converted to a structured telemetry document before storage.

```
bdscmd add-file-syslog <PATH>
```

| Argument | Description |
|---|---|
| `PATH` | Absolute path to the syslog file on the server's filesystem |

Monitor the `syslog_file_queue` and `syslog_file_name` fields in `v2/status`
to track progress.

**Example:**

```bash
# submit a syslog file generated by bdscli
bdscli generate syslog -n 1000 > /tmp/test.syslog
bdscmd add-file-syslog /tmp/test.syslog

# poll until the syslog queue drains
until [[ "$(bdscmd --raw status | jq '.syslog_file_queue')" -eq 0 ]]; do
  sleep 1
done

# verify with full-text search
bdscmd fulltext -q kernel -d 1h
```

**Output:**

```json
{
  "queued": "/tmp/test.syslog"
}
```

---

## 7. Commands — Inventory

### 7.1 `timeline`

Return the earliest and latest event timestamps stored across all shards.
Takes no arguments.

```
bdscmd timeline
```

**Example:**

```bash
bdscmd timeline
```

**Output:**

```json
{
  "min_ts": 1745500000,
  "max_ts": 1745603612
}
```

---

### 7.2 `count`

Count the total number of stored telemetry records. Without a time window all
records are counted.

```
bdscmd count [OPTIONS]
```

| Flag | Description |
|---|---|
| `-d, --duration <DUR>` | Lookback window, e.g. `"1h"`, `"30min"`, `"7d"` |
| `--start-ts <SECS>` | Range start as Unix seconds (pair with `--end-ts`) |
| `--end-ts <SECS>` | Range end as Unix seconds (pair with `--start-ts`) |

**Examples:**

```bash
# all time
bdscmd count

# last hour
bdscmd count -d 1h

# explicit range
bdscmd count --start-ts 1745500000 --end-ts 1745600000
```

**Output:**

```json
{
  "count": 8471
}
```

---

### 7.3 `shards`

List all shards with their time boundaries, filesystem path, and record counts.
Accepts the same time-window flags as `count`.

```
bdscmd shards [OPTIONS]
```

**Examples:**

```bash
# all shards
bdscmd shards

# shards active in the last 24h
bdscmd shards -d 24h

# shards in a specific range
bdscmd shards --start-ts 1745500000 --end-ts 1745600000
```

**Output:**

```json
{
  "shards": [
    {
      "id": "019735e2-...",
      "path": "/var/bds/db/shard-019735e2",
      "start_ts": 1745500000,
      "end_ts": 1745600000,
      "primaries": 4200,
      "secondaries": 1850
    }
  ]
}
```

---

## 8. Commands — Keys

### 8.1 `keys`

List the distinct sorted set of primary record keys seen in the given time window.

```
bdscmd keys --duration <DUR>
```

**Examples:**

```bash
bdscmd keys -d 1h
bdscmd keys -d 24h | jq '.keys[]'
```

**Output:**

```json
{
  "keys": ["cpu.usage", "disk.io", "mem.used", "net.rx"]
}
```

---

### 8.2 `keys-all`

List all keys matching a shell-glob pattern within a time window. Defaults to
`"*"` (all keys).

```
bdscmd keys-all --duration <DUR> [--key <PATTERN>]
```

| Flag | Default | Description |
|---|---|---|
| `-d, --duration` | required | Lookback window |
| `-k, --key` | `*` | Shell-glob pattern, e.g. `"cpu.*"`, `"disk.*"` |

**Examples:**

```bash
# all keys in the last hour
bdscmd keys-all -d 1h

# only CPU-related keys
bdscmd keys-all -d 1h -k 'cpu.*'

# keys matching a prefix
bdscmd keys-all -d 24h -k 'net.*'
```

---

### 8.3 `keys-get`

Retrieve primary record IDs and their associated secondary IDs for all keys
matching a pattern.

```
bdscmd keys-get --duration <DUR> --key <PATTERN>
```

**Example:**

```bash
bdscmd keys-get -d 1h -k 'cpu.usage'
```

**Output:**

```json
{
  "results": [
    {
      "primary_id": "019735e2-...",
      "timestamp": 1745603600,
      "secondary_ids": ["019735e3-...", "019735e4-..."]
    }
  ]
}
```

---

## 9. Commands — Primaries & Secondaries

### 9.1 `primaries`

Return the UUIDs of all primary records, optionally filtered by time window.

```
bdscmd primaries [OPTIONS]
```

Accepts the same time-window flags as `count`.

**Examples:**

```bash
bdscmd primaries -d 1h
bdscmd primaries --start-ts 1745500000 --end-ts 1745600000
```

---

### 9.2 `primaries-explore`

List keys that have more than one primary record in the window, together with the
count and UUIDs. Useful for understanding which keys are actively emitting events.

```
bdscmd primaries-explore --duration <DUR>
```

**Example:**

```bash
bdscmd primaries-explore -d 1h
```

**Output:**

```json
{
  "results": [
    {
      "key": "cpu.usage",
      "count": 60,
      "primary_id": ["019735e2-...", "019735e3-...", "..."]
    }
  ]
}
```

---

### 9.3 `primaries-explore-telemetry`

Like `primaries-explore` but restricted to keys whose primary records carry
numeric `data` — i.e. keys that are suitable for `trends` analysis.

```
bdscmd primaries-explore-telemetry --duration <DUR>
```

**Example:**

```bash
# discover which keys can be fed to `trends`
bdscmd primaries-explore-telemetry -d 1h | jq '.results[].key'
```

---

### 9.4 `primaries-get`

Retrieve the `data` payloads and timestamps for all primary records matching an
exact key.

```
bdscmd primaries-get --duration <DUR> --key <KEY>
```

**Example:**

```bash
bdscmd primaries-get -d 1h -k cpu.usage
```

**Output:**

```json
{
  "results": [
    {
      "id": "019735e2-...",
      "timestamp": 1745603540,
      "data": 0.73
    }
  ]
}
```

---

### 9.5 `primaries-get-telemetry`

Like `primaries-get` but extracts the numeric value from `data` or
`data["value"]`, returning a flat list of floats with timestamps. Used to feed
raw series into trend analysis scripts.

```
bdscmd primaries-get-telemetry --duration <DUR> --key <KEY>
```

**Example:**

```bash
bdscmd primaries-get-telemetry -d 1h -k cpu.usage | jq '.results[].value'
```

---

### 9.6 `primary`

Fetch the full stored document for a single primary record by UUID.

```
bdscmd primary <PRIMARY_ID>
```

**Example:**

```bash
bdscmd primary 019735e2-7c1a-7000-85fd-c17a3b8f912a
```

**Output:**

```json
{
  "id": "019735e2-7c1a-7000-85fd-c17a3b8f912a",
  "key": "cpu.usage",
  "timestamp": 1745603540,
  "data": 0.73,
  "host": "web-01",
  "secondary_count": 3,
  "duplications": []
}
```

---

### 9.7 `secondaries`

List the UUIDs of all secondary records associated with a primary.

```
bdscmd secondaries <PRIMARY_ID>
```

**Example:**

```bash
bdscmd secondaries 019735e2-7c1a-7000-85fd-c17a3b8f912a
```

---

### 9.8 `secondary`

Fetch the full stored document for a single secondary record by UUID.

```
bdscmd secondary <SECONDARY_ID>
```

**Example:**

```bash
bdscmd secondary 019735e3-7c1a-7000-85fd-c17a3b8f912b
```

**Output:**

```json
{
  "id": "019735e3-...",
  "primary_id": "019735e2-...",
  "key": "cpu.usage",
  "timestamp": 1745603541,
  "data": 0.74,
  "duplications": []
}
```

---

### 9.9 `duplicates`

Return a map of primary UUID → list of duplicate timestamps for all records that
were detected as exact-match duplicates in the time window.

```
bdscmd duplicates [OPTIONS]
```

Accepts the same time-window flags as `count`.

**Example:**

```bash
# find all duplicates from the last 6 hours
bdscmd duplicates -d 6h

# count how many primaries have duplicates
bdscmd --raw duplicates -d 24h | jq '.duplicates | length'
```

---

## 10. Commands — Search

### 10.1 `fulltext`

Full-text BM25 search over all indexed primary records in the time window.
Returns matching IDs and relevance scores.

```
bdscmd fulltext --query <QUERY> --duration <DUR> [--limit <N>]
```

| Flag | Default | Description |
|---|---|---|
| `-q, --query` | required | Search query |
| `-d, --duration` | required | Lookback window |
| `-l, --limit` | `10` | Maximum number of results |

**Examples:**

```bash
# find records mentioning "kernel panic"
bdscmd fulltext -q "kernel panic" -d 1h

# top 25 matches for "sshd" over the last day
bdscmd fulltext -q sshd -d 24h -l 25

# pipe IDs into primary lookups
bdscmd --raw fulltext -q "disk error" -d 6h | \
  jq -r '.results[].id' | \
  xargs -I{} bdscmd primary {}
```

**Output:**

```json
{
  "results": [
    { "id": "019735e2-...", "score": 4.21 },
    { "id": "019735e3-...", "score": 3.87 }
  ]
}
```

---

### 10.2 `fulltext-get`

Full-text search returning complete primary documents (not just IDs).

```
bdscmd fulltext-get --query <QUERY> --duration <DUR>
```

**Example:**

```bash
bdscmd fulltext-get -q "OOM killer" -d 2h | jq '.results[] | {key, data}'
```

**Output:**

```json
{
  "results": [
    {
      "key": "syslog",
      "timestamp": 1745603500,
      "data": "kernel: Out of memory: Kill process 1234...",
      "secondary_count": 0
    }
  ]
}
```

---

### 10.3 `fulltext-recent`

Full-text search returning results sorted by most recent timestamp first.
Useful for streaming the latest matching events.

```
bdscmd fulltext-recent --query <QUERY> --duration <DUR> [--limit <N>]
```

**Example:**

```bash
# latest 5 sshd authentication events
bdscmd fulltext-recent -q "authentication failure" -d 24h -l 5
```

**Output:**

```json
{
  "results": [
    { "id": "019735e9-...", "timestamp": 1745603610, "score": 3.14 },
    { "id": "019735e8-...", "timestamp": 1745603580, "score": 2.99 }
  ]
}
```

---

### 10.4 `search`

Semantic vector search using HNSW similarity. Returns IDs, timestamps, and
cosine-distance scores sorted by relevance.

```
bdscmd search --query <QUERY> --duration <DUR> [--limit <N>]
```

| Flag | Default | Description |
|---|---|---|
| `-q, --query` | required | Natural-language query |
| `-d, --duration` | required | Lookback window |
| `-l, --limit` | `10` | Maximum number of results |

**Examples:**

```bash
# find events semantically similar to a description
bdscmd search -q "high CPU utilisation on web servers" -d 1h

# broader search with more results
bdscmd search -q "network connectivity lost" -d 24h -l 20
```

**Output:**

```json
{
  "results": [
    { "id": "019735e2-...", "timestamp": 1745603540, "score": 0.97 },
    { "id": "019735e5-...", "timestamp": 1745603490, "score": 0.91 }
  ]
}
```

---

### 10.5 `search-get`

Semantic vector search returning full primary documents, sorted by timestamp.

```
bdscmd search-get --query <QUERY> --duration <DUR> [--limit <N>]
```

**Example:**

```bash
bdscmd search-get -q "database connection refused" -d 6h -l 5 | \
  jq '.results[] | {key, timestamp, data}'
```

---

## 11. Commands — Analysis

### 11.1 `trends`

Compute a statistical trend summary for a single key over a lookback window:
minimum, maximum, mean, median, standard deviation, detected anomalies, and
breakout events.

```
bdscmd trends --key <KEY> --duration <DUR>
```

**Examples:**

```bash
# trend summary for CPU usage over the last hour
bdscmd trends -k cpu.usage -d 1h

# extract just the anomaly timestamps
bdscmd --raw trends -k mem.used -d 24h | jq '.anomalies'
```

**Output:**

```json
{
  "key": "cpu.usage",
  "duration": "1h",
  "count": 60,
  "min": 0.12,
  "max": 0.98,
  "mean": 0.47,
  "median": 0.43,
  "std_dev": 0.18,
  "anomalies": [1745603400, 1745603460],
  "breakouts": []
}
```

**Workflow — discover then analyse:**

```bash
# step 1: find keys with multiple telemetry readings
bdscmd primaries-explore-telemetry -d 1h | jq -r '.results[].key'

# step 2: analyse each key
bdscmd trends -k cpu.usage -d 1h
bdscmd trends -k mem.used -d 1h
```

---

### 11.2 `topics`

Run LDA (Latent Dirichlet Allocation) topic modelling over the corpus of messages
stored under a specific key in the time window. Returns the top `--k` topics with
their keyword distributions.

```
bdscmd topics --key <KEY> --duration <DUR> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `-k, --key` | required | Key whose corpus to analyse |
| `-d, --duration` | required | Lookback window |
| `--k` | `3` | Number of topics to extract |
| `--alpha` | `0.1` | Document-topic Dirichlet prior |
| `--beta` | `0.01` | Topic-word Dirichlet prior |
| `--seed` | `42` | Random seed for reproducibility |
| `--iters` | `200` | LDA Gibbs sampling iterations |
| `--top-n` | `10` | Top N words per topic |

**Examples:**

```bash
# default topic extraction
bdscmd topics -k syslog -d 24h

# more topics, reproducible seed
bdscmd topics -k nginx.access -d 7d --k 5 --seed 123

# extract just the top words per topic
bdscmd --raw topics -k syslog -d 24h | \
  jq '.topics[] | {id: .topic_id, words: [.words[].word]}'
```

---

### 11.3 `topics-all`

Run LDA topic modelling across every distinct key in the time window, returning
one topic summary per key. Equivalent to calling `topics` for each key returned
by `keys`.

```
bdscmd topics-all --duration <DUR> [OPTIONS]
```

Accepts the same LDA tuning flags as `topics`.

**Examples:**

```bash
# default topics for every key in the last 24 hours
bdscmd topics-all -d 24h

# 5 topics, more iterations, compact output
bdscmd --raw topics-all -d 24h --k 5 --iters 500 | \
  jq '.topics[] | {key: .key, topics: [.topics[].words[0].word]}'
```

---

### 11.4 `rca`

Root cause analysis. Clusters non-telemetry event keys by co-occurrence within
time buckets, computes Jaccard similarity between clusters, and ranks probable
causes. Optionally anchors the analysis to a specific failure key.

```
bdscmd rca --duration <DUR> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `-d, --duration` | required | Lookback window |
| `-f, --failure-key` | none | Anchor key; global RCA if omitted |
| `--bucket-secs` | `300` | Co-occurrence bucket width in seconds |
| `--min-support` | `2` | Minimum bucket appearances to include a key |
| `--jaccard-threshold` | `0.2` | Minimum Jaccard score to link two keys |
| `--max-keys` | `200` | Maximum keys fed into the analysis |

**Examples:**

```bash
# global RCA across all events in the last 2 hours
bdscmd rca -d 2h

# anchor on a specific failure key
bdscmd rca -d 6h -f "payment.service.error"

# tighter clustering
bdscmd rca -d 24h --bucket-secs 60 --jaccard-threshold 0.5

# extract the ranked causes
bdscmd --raw rca -d 1h -f "db.connection.failed" | \
  jq '.causes[] | {key: .key, score: .score}'
```

---

### 11.5 `rca-templates`

Root cause analysis on drain3 log-template observations. Uses the same G-Forest
co-occurrence pipeline as `rca`, but operates on drain3 template bodies (e.g.
`"user <*> logged in from <*>"`) rather than raw event keys. Template events are
drawn from each shard's FrequencyTracking table, which records the timestamp of
every drain3 template store or update operation.

```
bdscmd rca-templates --duration <DUR> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `-d, --duration` | required | Lookback window, e.g. `"1h"`, `"2h"`, `"7days"` |
| `-f, --failure-body` | none | Exact drain3 pattern to anchor the analysis; runs global clustering when omitted |
| `--bucket-secs` | `300` | Co-occurrence bucket width in seconds |
| `--min-support` | `2` | Minimum distinct buckets a template must appear in |
| `--jaccard-threshold` | `0.2` | Minimum Jaccard similarity to link two templates |
| `--max-keys` | `200` | Maximum template bodies fed into the analysis |

**Examples:**

```bash
# global template clustering over the last 2 hours
bdscmd rca-templates -d 2h

# anchor on a specific failure template (use exact drain3 pattern)
bdscmd rca-templates -d 6h \
  --failure-body "service <*> crashed with exit code <*>"

# tighter clustering
bdscmd rca-templates -d 24h --jaccard-threshold 0.5 --bucket-secs 60

# extract probable causes
bdscmd --raw rca-templates -d 2h \
  --failure-body "disk <*> write error ENOSPC" | \
  jq '.probable_causes[] | {body, avg_lead_secs}'
```

**Output:**

```json
{
  "failure_body": "service <*> crashed with exit code <*>",
  "start": 1745600000,
  "end": 1745603600,
  "n_events": 15,
  "n_keys": 4,
  "clusters": [
    {
      "id": 0,
      "members": ["disk <*> usage <*>% warning threshold reached", "disk <*> write error ENOSPC", "service <*> crashed with exit code <*>"],
      "support": 3,
      "cohesion": 1.0
    }
  ],
  "probable_causes": [
    {
      "body": "disk <*> usage <*>% warning threshold reached",
      "co_occurrence_count": 3,
      "jaccard": 1.0,
      "avg_lead_secs": 120.0
    }
  ]
}
```

Use `tpl-templates-recent` to discover available drain3 template body strings
before constructing the `--failure-body` argument.

---

## 12. Commands — Template Store

The template store (`tplstorage`) holds drain3 log-template documents: named
pattern strings (e.g. `"user <*> logged in from <*>"`) with associated metadata
and a vector index. Templates are created either manually via `tpl-add` or
automatically by the drain3 miner when `drain_enabled = true` in the bdsnode
config.

Each shard has its own tplstorage, and FrequencyTracking records the Unix timestamp
of every template observation event.  The `tpl-template-by-id`,
`tpl-templates-by-timestamp`, and `tpl-templates-recent` commands query the
FrequencyTracking layer across all shards.

---

### 12.1 `tpl-add`

Store a drain3 template document manually.

```
bdscmd tpl-add --name <NAME> --body <BODY> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `-n, --name` | required | Human-readable template name |
| `-b, --body` | required | Template body text (drain3 pattern) |
| `-t, --timestamp` | current time | Unix seconds; determines the target shard |
| `--tag <TAG>` | none | Tag label (may be repeated) |
| `-d, --description` | `""` | Optional description |

**Example:**

```bash
bdscmd tpl-add \
  --name "Auth login" \
  --body "user <*> logged in from <*>" \
  --tag auth --tag login \
  --description "SSH/PAM login template"
```

**Output:**

```json
{ "id": "019735e2-7c1a-7000-85fd-c17a3b8f912a" }
```

---

### 12.2 `tpl-get`

Fetch a template document by UUID.

```
bdscmd tpl-get --id <UUID>
```

**Example:**

```bash
bdscmd tpl-get --id 019735e2-7c1a-7000-85fd-c17a3b8f912a
```

**Output:**

```json
{
  "id": "019735e2-7c1a-7000-85fd-c17a3b8f912a",
  "metadata": {
    "name": "Auth login",
    "tags": ["auth", "login"],
    "description": "SSH/PAM login template",
    "type": "template",
    "timestamp": 1745600000,
    "created_at": 1745600000
  },
  "body": "user <*> logged in from <*>"
}
```

---

### 12.3 `tpl-delete`

Delete a template document by UUID. Idempotent.

```
bdscmd tpl-delete --id <UUID>
```

**Example:**

```bash
bdscmd tpl-delete --id 019735e2-7c1a-7000-85fd-c17a3b8f912a
```

**Output:**

```json
{ "deleted": true }
```

---

### 12.4 `tpl-list`

List template documents discovered within a lookback window.

```
bdscmd tpl-list [--duration <DUR>]
```

| Flag | Default | Description |
|---|---|---|
| `-d, --duration` | `1h` | Lookback window |

**Example:**

```bash
# list templates discovered in the last 2 hours
bdscmd tpl-list -d 2h

# extract just the body strings
bdscmd --raw tpl-list -d 24h | jq '.templates[].metadata.name'
```

**Output:**

```json
{
  "templates": [
    {
      "id": "019735e2-...",
      "metadata": { "name": "Auth login", "timestamp": 1745600000, ... }
    }
  ]
}
```

---

### 12.5 `tpl-search`

Semantic vector search over template documents.

```
bdscmd tpl-search --query <QUERY> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `-q, --query` | required | Natural-language query |
| `-d, --duration` | `1h` | Lookback window for shards to search |
| `-l, --limit` | `10` | Maximum results |

**Example:**

```bash
bdscmd tpl-search -q "disk full error" -d 24h -l 5
```

**Output:**

```json
{
  "results": [
    { "id": "019735e2-...", "score": 0.94, "body": "disk <*> write error ENOSPC" }
  ]
}
```

---

### 12.6 `tpl-update`

Update a template document's metadata or body in-place.

```
bdscmd tpl-update --id <UUID> [OPTIONS]
```

| Flag | Default | Description |
|---|---|---|
| `-i, --id` | required | UUID v7 of the template |
| `-n, --name` | unchanged | New name |
| `-b, --body` | unchanged | New body text |
| `--tag <TAG>` | unchanged | Replace tag list (repeat for multiple; omit to leave unchanged) |
| `-d, --description` | unchanged | New description |

**Example:**

```bash
bdscmd tpl-update \
  --id 019735e2-7c1a-7000-85fd-c17a3b8f912a \
  --name "SSH Auth login" \
  --tag auth --tag ssh
```

**Output:**

```json
{ "updated": true }
```

---

### 12.7 `tpl-reindex`

Rebuild the template store vector index from persisted metadata and blobs.
Use after unclean shutdown or bulk updates.

```
bdscmd tpl-reindex [--duration <DUR>]
```

| Flag | Default | Description |
|---|---|---|
| `-d, --duration` | `24h` | Lookback window for shards to reindex |

**Example:**

```bash
bdscmd tpl-reindex -d 7days
```

**Output:**

```json
{ "indexed": 42 }
```

---

### 12.8 `tpl-template-by-id`

Fetch a template document (with `id`, `metadata`, and `body`) by UUID via the
FrequencyTracking cross-shard lookup. Scans all shards; returns `null` when the
UUID is not found.

```
bdscmd tpl-template-by-id --id <UUID>
```

**Example:**

```bash
bdscmd tpl-template-by-id --id 019735e2-7c1a-7000-85fd-c17a3b8f912a
```

**Output:**

```json
{
  "template": {
    "id": "019735e2-7c1a-7000-85fd-c17a3b8f912a",
    "metadata": { "name": "Auth login", "timestamp": 1745600000, "type": "tpl" },
    "body": "user <*> logged in from <*>"
  }
}
```

---

### 12.9 `tpl-templates-by-timestamp`

List all template documents whose FrequencyTracking observation timestamp falls
within an inclusive `[start_ts, end_ts]` range. Queries all shards and
deduplicates by UUID.

```
bdscmd tpl-templates-by-timestamp --start-ts <SECS> --end-ts <SECS>
```

| Flag | Description |
|---|---|
| `-s, --start-ts` | Range start as Unix seconds (inclusive) |
| `-e, --end-ts` | Range end as Unix seconds (inclusive) |

**Example:**

```bash
bdscmd tpl-templates-by-timestamp --start-ts 1745600000 --end-ts 1745603600
```

**Output:**

```json
{
  "templates": [
    {
      "id": "019735e2-...",
      "metadata": { "name": "Auth login", "timestamp": 1745600100, "type": "tpl" },
      "body": "user <*> logged in from <*>"
    }
  ]
}
```

---

### 12.10 `tpl-templates-recent`

List all template documents whose FrequencyTracking observation falls within a
humantime lookback window. Equivalent to `tpl-templates-by-timestamp` with
automatically computed bounds.

```
bdscmd tpl-templates-recent [--duration <DUR>]
```

| Flag | Default | Description |
|---|---|---|
| `-d, --duration` | `1h` | Lookback window, e.g. `"1h"`, `"30min"`, `"7days"` |

**Example:**

```bash
# templates observed in the last 2 hours
bdscmd tpl-templates-recent -d 2h

# extract body strings for use with rca-templates
bdscmd --raw tpl-templates-recent -d 6h | jq -r '.templates[].body'
```

**Output:**

```json
{
  "templates": [
    {
      "id": "019735e2-...",
      "metadata": { "name": "Auth login", "timestamp": 1745600100, "type": "tpl" },
      "body": "user <*> logged in from <*>"
    }
  ]
}
```

---

## 13. Commands — BUND VM

### 13.1 `eval`

Compile and evaluate a BUND stack-based script in a named VM context. The result
is the workbench stack printed as a JSON array.

```
bdscmd eval [OPTIONS] [SOURCE]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `SOURCE` | stdin | Path to a `.bund` file, `-` for stdin, or omit to read from stdin |
| `-c, --context` | `default` | Name of the BUND VM context to use |

**Examples:**

```bash
# inline script from stdin
echo '1 2 + .' | bdscmd eval

# from a file
bdscmd eval my_script.bund

# explicit stdin marker
cat my_script.bund | bdscmd eval -

# named context (isolates state between sessions)
bdscmd eval -c analytics my_analytics.bund

# heredoc
bdscmd eval <<'BUND'
  "syslog" duration 1h topics
BUND
```

**Output:**

```json
[42, "hello", [1, 2, 3]]
```

---

### 13.2 Shebang Scripts

`bdscmd eval` supports the Unix shebang mechanism, allowing BUND scripts to be
executed directly as programs. The kernel passes the script path as the first
positional argument to the interpreter.

Add a shebang line as the first line of the script:

```
#!/usr/local/bin/bdscmd eval
```

The `#!` line is automatically stripped before the script is sent to the VM, so
it does not affect execution.

**Example script — `analyse_syslog.bund`:**

```
#!/usr/local/bin/bdscmd eval
"syslog" "1h" topics
```

Make it executable and run it directly:

```bash
chmod +x analyse_syslog.bund
./analyse_syslog.bund
```

**Shebang with a non-default context:**

The shebang line only specifies the interpreter path and subcommand; additional
flags like `--context` must be set via the `BDSCMD_ADDR` / `BDSCMD_SESSION`
environment variables or by wrapping the script in a shell script that invokes
`bdscmd eval --context myctx <script>` directly.

**Alternative: pipe to eval**

For one-liners and pipelines that should not touch the filesystem:

```bash
# generate a BUND script from another tool and pipe it in
generate_report.sh | bdscmd eval -c reporting
```

---

## 14. Quick Reference

| Subcommand | JSON-RPC method | Key parameters |
|---|---|---|
| `status` | `v2/status` | — |
| `add` | `v2/add` | `DOC` |
| `add-batch` | `v2/add.batch` | `SOURCE` |
| `add-file` | `v2/add.file` | `PATH` |
| `add-file-syslog` | `v2/add.file.syslog` | `PATH` |
| `timeline` | `v2/timeline` | — |
| `count` | `v2/count` | `-d`, `--start-ts`, `--end-ts` |
| `shards` | `v2/shards` | `-d`, `--start-ts`, `--end-ts` |
| `keys` | `v2/keys` | `-d` |
| `keys-all` | `v2/keys.all` | `-d`, `-k` |
| `keys-get` | `v2/keys.get` | `-d`, `-k` |
| `primaries` | `v2/primaries` | `-d`, `--start-ts`, `--end-ts` |
| `primaries-explore` | `v2/primaries.explore` | `-d` |
| `primaries-explore-telemetry` | `v2/primaries.explore.telemetry` | `-d` |
| `primaries-get` | `v2/primaries.get` | `-d`, `-k` |
| `primaries-get-telemetry` | `v2/primaries.get.telemetry` | `-d`, `-k` |
| `primary` | `v2/primary` | `PRIMARY_ID` |
| `secondaries` | `v2/secondaries` | `PRIMARY_ID` |
| `secondary` | `v2/secondary` | `SECONDARY_ID` |
| `duplicates` | `v2/duplicates` | `-d`, `--start-ts`, `--end-ts` |
| `fulltext` | `v2/fulltext` | `-q`, `-d`, `-l` |
| `fulltext-get` | `v2/fulltext.get` | `-q`, `-d` |
| `fulltext-recent` | `v2/fulltext.recent` | `-q`, `-d`, `-l` |
| `search` | `v2/search` | `-q`, `-d`, `-l` |
| `search-get` | `v2/search.get` | `-q`, `-d`, `-l` |
| `trends` | `v2/trends` | `-k`, `-d` |
| `topics` | `v2/topics` | `-k`, `-d`, `--k`, `--iters`, `--top-n` |
| `topics-all` | `v2/topics.all` | `-d`, `--k`, `--iters`, `--top-n` |
| `rca` | `v2/rca` | `-d`, `-f`, `--bucket-secs`, `--jaccard-threshold` |
| `rca-templates` | `v2/rca.templates` | `-d`, `-f`, `--bucket-secs`, `--jaccard-threshold` |
| `tpl-add` | `v2/tpl.add` | `-n`, `-b`, `-t`, `--tag`, `-d` |
| `tpl-get` | `v2/tpl.get` | `-i` |
| `tpl-delete` | `v2/tpl.delete` | `-i` |
| `tpl-list` | `v2/tpl.list` | `-d` |
| `tpl-search` | `v2/tpl.search` | `-q`, `-d`, `-l` |
| `tpl-update` | `v2/tpl.update` | `-i`, `-n`, `-b`, `--tag`, `-d` |
| `tpl-reindex` | `v2/tpl.reindex` | `-d` |
| `tpl-template-by-id` | `v2/tpl.template_by_id` | `-i` |
| `tpl-templates-by-timestamp` | `v2/tpl.templates_by_timestamp` | `-s`, `-e` |
| `tpl-templates-recent` | `v2/tpl.templates_recent` | `-d` |
| `eval` | `v2/eval` | `SOURCE`, `-c` |

---

## 15. Exit Codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Error: argument parsing failure, server pre-flight check failed, server returned a JSON-RPC error, I/O error |
