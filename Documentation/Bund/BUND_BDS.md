# BUND_BDS — Cluster-aware BDS words (`cls.*`)

Reference for every BUND word provided by `src/vm/stdlib/cluster`.
These words mirror the local `db.*` / `doc.*` family
([`BDS.md`](BDS.md)) but transparently fan out across cluster
peers when cluster mode is on.  In **standalone mode** every
`cls.*` word degrades to a local-only call — the script doesn't
change.

**Stack-effect notation:** `( before -- after )` where the top of
the stack is on the right.  `W:x` denotes a value on the
**workbench** instead of the stack.  A trailing `.` on a word name
means the word reads from and writes to the **workbench** instead
of the main stack.  Both forms always co-exist — registering
`cls.add` also registers `cls.add.`.

**Map construction.**  `dict` pushes the empty map; each
`key value set` triple inserts and leaves the updated map on top.
Example:

```bund
dict
"key"       "cpu.user"    set
"timestamp" 1700000000    set
"data"      "ping"        set
// stack top: {"key":"cpu.user", "timestamp":1700000000, "data":"ping"}
```

**Result extraction.**  Reads return Maps shaped like
`{"results": [...]}`, `{"count": N, ...}`, etc.  Use `"results" $get`
(or `$get` in the workbench form) to pull a field out.

**Three replication strategies** are baked into the helpers; the
strategy is fixed per word, not configurable:

| Strategy                  | Words                                                                                   | Wire path                                                                 |
|---------------------------|-----------------------------------------------------------------------------------------|---------------------------------------------------------------------------|
| **Sharded write**         | `cls.add`, `cls.add.batch`, `cls.update`, `cls.delete`                                  | Local commit + fan-out to `replication_factor - 1` random Alive peers; hint on failure |
| **Fully-replicated write** | `cls.doc.*` (writes), `cls.tpl.*` (writes), `cls.signal.{emit,update}`, `cls.script.*` (writes) | Local commit + fan-out to **every** Alive peer; hint on failure          |
| **Fan-out read**          | Every `cls.*` read except local-only ones (see below)                                   | Local call + parallel calls to every Alive peer; merged via `cluster::merge` |
| **Local-only read**       | `cls.doc.get.*`, `cls.signal.get`, `cls.scripts.list`, `cls.script.get`, …              | The store is fully-replicated, so anti-entropy guarantees the local copy is complete |

After **any** `cls.*` call, the per-thread `?cluster.meta`
introspection word ([§ 12](#12-introspection-cluster-meta--llm-meta))
holds the fan-out / replication summary of the most-recent call.

---

## Table of contents

1. [Ingest — `cls.add`](#1-ingest--clsadd)
2. [Inventory — `cls.count` / `cls.duplicates` / `cls.fingerprints.recent`](#2-inventory--clscount--clsduplicates--clsfingerprintsrecent)
3. [Search — `cls.search` / `cls.fulltext` / `cls.aggregation`](#3-search--clssearch--clsfulltext--clsaggregation)
4. [Keys & records — `cls.keys` / `cls.primaries` / `cls.secondary*`](#4-keys--records--clskeys--clsprimaries--clssecondary)
5. [Analytics — `cls.knn` / `cls.anomaly.recent` / `cls.denoise.recent`](#5-analytics--clsknn--clsanomalyrecent--clsdenoiserecent)
6. [Trends, RCA, topics, summaries, timeline](#6-trends-rca-topics-summaries-timeline)
7. [Signals — `cls.signal.*` / `cls.signals.*`](#7-signals--clssignal--clssignals)
8. [Document store — `cls.doc.*`](#8-document-store--clsdoc)
9. [Template store — `cls.tpl.*`](#9-template-store--clstpl)
10. [Script store — `cls.script.*` / `cls.scripts.list`](#10-script-store--clsscript--clsscriptslist)
11. [LLM — `cls.llm.*`](#11-llm--clsllm)
12. [Introspection — `?cluster.meta` / `?llm.meta`](#12-introspection-cluster-meta--llm-meta)
13. [End-to-end recipe](#13-end-to-end-recipe)
14. [Quick reference — every cls.* word](#14-quick-reference)

---

## 1. Ingest — `cls.add`

### `cls.add` / `cls.add.`

```
( doc:MAP -- id:STRING )
( W:doc:MAP -- W:id:STRING )
```

Sharded write.  Stores `doc` locally and replicates to
`replication_factor - 1` random Alive peers.  Failed replicas
enqueue hints in the shared hint storage; the cluster's background
task replays them when peers transition back to Alive.

```bund
dict
"key"       "cpu.user"        set
"timestamp" 1700000000        set
"data"      "from cls demo"   set
cls.add
// stack: "019e21f6-7a3e-…"  (the new UUID)
?cluster.meta
// {enabled:true, replication: {peers_attempted:2, peers_succeeded:2, hints_queued:0}}
```

### `cls.add.batch` / `cls.add.batch.`

```
( docs:LIST<MAP> -- ids:LIST<STRING> )
```

Same as `cls.add` but ingests a list in one round-trip per peer.
The order of `ids` matches the order of `docs` on input.

```bund
[ dict "key" "m1" set "timestamp" 1700000000 set "data" 1 set
  dict "key" "m2" set "timestamp" 1700000001 set "data" 2 set
] cls.add.batch
```

### `cls.update` / `cls.update.`

```
( id:STR  doc:MAP -- new_id:STRING )
```

Replace a record's payload (sharded write — same fan-out as
`cls.add`).  Caller hands back the new UUID.

### `cls.delete` / `cls.delete.`

```
( id:STR -- nodata )
```

Sharded delete (no tombstone in shard storage — duplicates resolve
by latest-write semantics on the per-shard ObservabilityStorage).

---

## 2. Inventory — `cls.count` / `cls.duplicates` / `cls.fingerprints.recent`

### `cls.count` / `cls.count.`

```
( opts:MAP -- {count:INT, local_count:INT, ...} )
```

`opts` accepts `{"duration": "1h"}`, `{"start_ts":…,"end_ts":…}`, or
empty for "every shard ever".  The cluster fan-out sums the
per-peer `count` fields; fully-replicated data therefore over-counts
by ~RF — divide by `replication_factor` to estimate true unique
records, or use `cls.primaries` (UUID-deduped).

```bund
dict "duration" "1h" set cls.count
// stack: {count: 12480, local_count: 4160, ...}
```

### `cls.duplicates` / `cls.duplicates.`

```
( opts:MAP -- {id:STR -> [ts:INT, …], ...} )
```

Per-primary list of every duplicate observation timestamp.  Used by
the anti-entropy verifier and incident-investigation scripts.

### `cls.fingerprints.recent` / `cls.fingerprints.recent.`

```
( duration:STR -- {fingerprints: [...]} )
```

Cluster-wide UUID + fingerprint pairs for every primary in the
window.  Input for offline k-NN / anomaly / denoise analysis that
runs on a different node.

---

## 3. Search — `cls.search` / `cls.fulltext` / `cls.aggregation`

Every read in this family fans out and merges per-peer hits by UUID
dedup with score average.

### `cls.search` / `cls.search.`

```
( query:any  duration:STR -- {results:LIST} )
```

Vector semantic search.  `query` may be a STRING or MAP — both are
fingerprinted and embedded automatically.

```bund
"login failures" "1h" cls.search
?cluster.meta              // {peers_queried:2, peers_answered:2, partial:false, ...}
```

### `cls.search.get` / `cls.search.get.`

```
( query:any  duration:STR  limit:INT -- {results:LIST<MAP>} )
```

Same as `cls.search` but returns the full primary documents (not
just `(id, score)` pairs).  Server-side limit per peer is `limit`;
post-merge the result is truncated to `limit`.

### `cls.search.fts` / `cls.search.fts.`

```
( query:STR  duration:STR -- {results:LIST} )
```

BM25 full-text search.  Fast, surfaces lexical matches that the
vector path misses.

### `cls.fulltext` / `cls.fulltext.`

```
( query:STR  duration:STR  limit:INT -- {results:[{id,score}, ...]} )
```

`cls.search.fts` flavour returning ranked `(id, score)` pairs.

### `cls.fulltext.recent` / `cls.fulltext.recent.`

```
( query:STR  duration:STR  limit:INT -- {results:[{id,ts,score}, ...]} )
```

Same as `cls.fulltext` but sorted newest-first by primary timestamp
(handy for "what just happened" investigations).

### `cls.fulltext.get` / `cls.fulltext.get.`

```
( query:STR  duration:STR  limit:INT -- {results:LIST<MAP>} )
```

Returns full primary documents instead of `(id, score)`.

### `cls.aggregation` / `cls.aggregation.`

```
( query:STR  duration:STR -- {observability:LIST, documents:LIST} )
```

Combined vector search over telemetry shards **and** the cluster
document store in one round-trip per peer.  The closest thing
bdsnode has to a "search everything" call.

```bund
"network outage" "1h" cls.aggregation
// {observability: [{...}, ...], documents: [{...}, ...]}
```

---

## 4. Keys & records — `cls.keys` / `cls.primaries` / `cls.secondary*`

### `cls.keys` / `cls.keys.`

```
( duration:STR -- {keys:[STR, ...]} )
```

Cluster-wide sorted union of telemetry keys observed in the window.

### `cls.keys.all` / `cls.keys.all.`

```
( duration:STR  pattern:STR -- {keys:[STR, ...]} )
```

Shell-glob-filtered key enumeration (e.g. `"cpu.*"`).

### `cls.keys.get` / `cls.keys.get.`

```
( duration:STR  key:STR -- {results:LIST<MAP>} )
```

Per-key primary IDs + secondary lists, cluster-wide.

### `cls.primaries` / `cls.primaries.`

```
( opts:MAP -- {ids:[STR, ...]} )
```

UUIDs of every primary record matching `opts`
(`{"duration": "1h"}`, `{"start_ts":…,"end_ts":…}`, or empty for
"every shard ever").  Sorted and deduplicated across peers.

```bund
dict "duration" "6h" set cls.primaries
// {ids: ["019e...", "019e...", ...]}
```

### `cls.primaries.explore` / `cls.primaries.explore.`

```
( duration:STR -- {results:LIST<MAP>} )
```

Per-key UUID rollup with sample counts.  Each row:
`{key, count, ids:[STR,...]}`.

### `cls.primaries.explore.telemetry` / `cls.primaries.explore.telemetry.`

Same shape as `cls.primaries.explore` but restricted to keys whose
records have numeric `data` payloads — pre-filtered for
`cls.trends`.

### `cls.primaries.get` / `cls.primaries.get.`

```
( duration:STR  key:STR -- {results:LIST<MAP>} )
```

Full primary documents for one exact key in the window.

### `cls.primaries.get.telemetry` / `cls.primaries.get.telemetry.`

Same as `cls.primaries.get` but returns just the extracted numeric
values + timestamps — feed straight into `cls.trends`.

### `cls.primary` / `cls.primary.`

```
( id:STR -- record:MAP )
```

Fetch a single primary by UUID, cluster-wide first-non-null wins.

### `cls.secondary` / `cls.secondary.`

```
( id:STR -- record:MAP )
```

Same, for a single secondary record.

### `cls.secondaries` / `cls.secondaries.`

```
( primary_id:STR -- {ids:[STR, ...]} )
```

Every secondary UUID linked to the given primary, cluster-wide.

---

## 5. Analytics — `cls.knn` / `cls.anomaly.recent` / `cls.denoise.recent`

Corpus-based analyses.  The coordinator fans out
`v2/fingerprints.recent` to every peer, unions and dedupes the
result by UUID, then runs the analysis **once** on the merged
corpus.

### `cls.knn` / `cls.knn.`

```
( duration:STR  opts:MAP -- {clusters, representatives, outliers, ...} )
```

TF-IDF + cosine-density clustering.  `opts` accepts
`{"k":INT, "min_word_len":INT, "outlier_threshold":FLOAT}`; pass
`dict` (empty Map) to use defaults.

```bund
"1h" dict cls.knn
?cluster.meta              // {peers_queried:2, peers_answered:2, ...}
```

### `cls.anomaly.recent` / `cls.anomaly.recent.`

```
( duration:STR  opts:MAP -- {anomalies:LIST, ...} )
```

N-gram phrase-rarity outlier detection.

### `cls.denoise.recent` / `cls.denoise.recent.`

```
( duration:STR  opts:MAP -- {kept:LIST, removed:LIST, ...} )
```

Splits the recent corpus into `kept` (signal) and `removed` (noise)
based on the same n-gram-rarity scoring.

---

## 6. Trends, RCA, topics, summaries, timeline

For corpus-relative outputs that aren't directly mergeable (LDA,
RCA, statistical trends), the helper picks the response with the
largest sample / event / corpus count.

### `cls.timeline` / `cls.timeline.`

```
( -- {min_ts:INT, max_ts:INT} )
```

Earliest + latest event timestamps across the cluster.  Cheapest
read in the family — handy for "is anything ingesting?" smoke tests.

### `cls.trends` / `cls.trends.`

```
( key:STR  duration:STR -- {min, max, mean, median, std_dev, anomalies, breakouts, ...} )
```

Statistical trend summary for one telemetry key.  Cluster fan-out
picks the response with the largest sample count.

### `cls.rca` / `cls.rca.`

```
( opts:MAP -- {clusters:LIST, ranked_causes:LIST, ...} )
```

Root-cause analysis over non-telemetry events.  `opts` accepts
`{"duration":"1h", "failure_key":"…", "bucket_secs":60,
  "min_support":3, "jaccard_threshold":0.5, "max_keys":20}`.

```bund
dict
"duration"          "30m"           set
"failure_key"       "service.error" set
"bucket_secs"       60              set
"jaccard_threshold" 0.5             set
cls.rca
```

### `cls.rca.templates` / `cls.rca.templates.`

Same shape as `cls.rca` but operates on drain3 templates instead of
raw events.  `failure_key` is matched against template bodies /
metadata names.

### `cls.topics` / `cls.topics.`

```
( opts:MAP -- {topic:STR, terms:[...], ...} )
```

LDA topic modelling for a single key's corpus.  `opts` needs
`{"key":"...", "duration":"1h"}` and optionally
`{"k":INT, "iters":INT, "top_n":INT}`.

### `cls.topics.all` / `cls.topics.all.`

```
( opts:MAP -- {topics:LIST} )
```

LDA for every distinct key in the window — one result per key.

### `cls.textrank.templates` / `cls.textrank.templates.`

```
( duration:STR  opts:MAP -- {summary:STR, sentences:LIST, ...} )
```

Extractive TextRank summary over the templates observed in the
window.  `opts`: `{"max_sentences":INT, "ratio":FLOAT,
"min_word_len":INT}`.

### `cls.summary.recent` / `cls.summary.recent.`

```
( duration:STR  opts:MAP -- {summary:STR, ...} )
```

TextRank summary of text-bearing primary records in the window —
strips numeric measurements, extracts bodies from
`data["value"]` or `data["raw"]`.

### `cls.summary.query` / `cls.summary.query.`

```
( query:STR  opts:MAP -- {summary:STR, ...} )
```

Same body extraction as `cls.summary.recent`, but the corpus is the
records matching a vector query (default lookback 365 d unless
`opts.duration` overrides).

### `cls.summary.lsa.recent` / `cls.summary.lsa.recent.`

LSA-based replacement for `cls.summary.recent` — SVD-based
Steinberger-Ježek scoring.

### `cls.summary.lsa.query` / `cls.summary.lsa.query.`

LSA-based replacement for `cls.summary.query`.

---

## 7. Signals — `cls.signal.*` / `cls.signals.*`

Signals are the fully-replicated store for named events with
arbitrary metadata — alerts, build markers, deployment events,
RCA findings.  Writes replicate to **every** Alive peer.

### `cls.signal.emit` / `cls.signal.emit.`

```
( name:STR  severity:STR  ts:INT  extra:MAP -- id:STRING )
```

Arguments are deepest-first: `name`, `severity`, `ts`, then the
metadata Map.

```bund
"smoke.signal"    // name
"info"            // severity ("info" / "warn" / "error" / "critical")
1700000000        // unix-seconds timestamp
dict
  "source"   "rca-script"   set
  "evidence" "k=…"          set
cls.signal.emit
// stack: "019e...-..."  (the new signal UUID)
```

### `cls.signal.update` / `cls.signal.update.`

```
( id:STR  metadata:MAP -- nodata )
```

Replace the signal's metadata in place (full overwrite, not merge).

### `cls.signal.get` / `cls.signal.get.`

```
( id:STR -- metadata:MAP | nodata )
```

Local-only fetch by UUID.

### `cls.signals.recent` / `cls.signals.recent.`

```
( duration:STR -- {count:INT, signals:LIST<MAP>} )
```

Cluster-wide signal list for the window.

### `cls.signals.query` / `cls.signals.query.`

```
( query:STR  limit:INT -- {count:INT, results:LIST<MAP>} )
```

Semantic search over the signal store by plain-text query.

```bund
"deployment errors last night" 20 cls.signals.query
```

---

## 8. Document store — `cls.doc.*`

The document store is **fully replicated** across every peer.
Writes (`add`, `add.file`, `update.*`, `delete`, `reindex`, `sync`)
fan out; reads (`get.*`, `search.*`) run local because anti-entropy
guarantees the local copy is complete.

### `cls.doc.add` / `cls.doc.add.`

```
( metadata:MAP  content:BIN|STR -- id:STRING )
```

`content` may be raw bytes or a string (auto-converted to UTF-8).
Metadata is free-form JSON; the loader script
`load_internal_documentation.sh` tags every internal doc with
`internal_doc: true` so consumers (the bdsweb Help page, `v3/help`
with `internal_only=true`) can scope to the curated corpus.

```bund
dict
"name"     "incident-2026-05-12.md"   set
"category" "postmortem"                set
"hello from cls.doc.add"
cls.doc.add
```

### `cls.doc.add.file` / `cls.doc.add.file.`

```
( metadata:MAP  path:STR -- id:STRING )
```

Load a file from disk, chunk it semantically (sentence /
paragraph), and store each chunk as an independently searchable
record linked from the document-level metadata.  See
[`DOCUMENTSENGINE.md`](../DOCUMENTSENGINE.md) for the chunking
algorithm.

### `cls.doc.update.metadata` / `cls.doc.update.metadata.`

```
( id:STR  metadata:MAP -- nodata )
```

Full overwrite of the metadata record (vector index is **not**
updated automatically — call `cls.doc.reindex` after bulk metadata
edits).

### `cls.doc.update.content` / `cls.doc.update.content.`

```
( id:STR  content:BIN|STR -- nodata )
```

Replace the content blob in place.  Same reindex caveat.

### `cls.doc.delete` / `cls.doc.delete.`

```
( id:STR -- nodata )
```

Removes metadata + blob + vector entry across the cluster.  Leaves
a tombstone so anti-entropy doesn't resurrect.

### `cls.doc.get.metadata` / `cls.doc.get.metadata.`

```
( id:STR -- metadata:MAP | nodata )
```

Local-only metadata fetch.

### `cls.doc.get.content` / `cls.doc.get.content.`

```
( id:STR -- bytes:BIN )
```

Local-only content fetch.

### `cls.doc.search` / `cls.doc.search.`

```
( query:STR|MAP  limit:INT -- {results:LIST<MAP>} )
```

Plain-text or JSON semantic search.  Each result carries
`{id, metadata, document, score}`.

### `cls.doc.search.strings` / `cls.doc.search.strings.`

```
( query:STR|MAP  limit:INT -- {results:[STR, ...]} )
```

Same retrieval, returns each hit as a `json_fingerprint` string —
saves the caller a flattening step when feeding LLM RAG context.

### `cls.doc.search.json` / `cls.doc.search.json.`

```
( query:MAP  limit:INT -- {results:LIST<MAP>} )
```

JSON-shaped query input (alternate to plain-text).

### `cls.doc.search.json.strings` / `cls.doc.search.json.strings.`

JSON query + fingerprint-string output combined.

### `cls.doc.reindex` / `cls.doc.reindex.`

```
( -- n_reindexed:INT )
```

Rebuild the HNSW vector index from the persisted metadata + blobs.
Run after a bulk content update or unclean shutdown.

### `cls.doc.sync` / `cls.doc.sync.`

```
( -- nodata )
```

Force a docstore CHECKPOINT.

---

## 9. Template store — `cls.tpl.*`

Drain3 templates are the structured-log representation: a template
body is a parameterised pattern that summarises many concrete log
lines.  Like `cls.doc.*`, the template store is fully replicated.

### `cls.tpl.add` / `cls.tpl.add.`

```
( metadata:MAP  body:BIN|STR -- id:STRING )
```

`metadata` typically carries `{"name":"...", "tags":[...]}`.

### `cls.tpl.update.metadata` / `cls.tpl.update.metadata.`

```
( id:STR  metadata:MAP -- nodata )
```

### `cls.tpl.update.body` / `cls.tpl.update.body.`

```
( id:STR  body:BIN|STR -- nodata )
```

### `cls.tpl.delete` / `cls.tpl.delete.`

```
( id:STR -- nodata )
```

### `cls.tpl.reindex` / `cls.tpl.reindex.`

```
( duration:STR -- n_reindexed:INT )
```

Per-shard HNSW rebuild over the window.

### `cls.tpl.get` / `cls.tpl.get.`

```
( id:STR -- {id, metadata, body} )
```

### `cls.tpl.list` / `cls.tpl.list.`

```
( duration:STR -- {templates:LIST<MAP>} )
```

Every template (manual + drain3-mined) in shards overlapping the
window, metadata only.

### `cls.tpl.search` / `cls.tpl.search.`

```
( duration:STR  query:STR  limit:INT -- {results:LIST<MAP>} )
```

Semantic vector search over templates within the window.

### `cls.tpl.template.by.id` / `cls.tpl.template.by.id.`

```
( id:STR -- {template:MAP} )
```

Cluster-wide single-template fetch.

### `cls.tpl.templates.recent` / `cls.tpl.templates.recent.`

```
( duration:STR -- {templates:LIST<MAP>} )
```

Drain3 templates whose FrequencyTracking observation falls within
the trailing window.

### `cls.tpl.templates.by.timestamp` / `cls.tpl.templates.by.timestamp.`

```
( start_ts:INT  end_ts:INT -- {templates:LIST<MAP>} )
```

Same shape but with an explicit Unix-second range.

---

## 10. Script store — `cls.script.*` / `cls.scripts.list`

Stored BUND scripts are fully replicated; the cluster-aware
Scheduler runs them on a crontab schedule with dedup so two
coordinators don't fire the same script.  See
[`../CLUSTER.md`](../CLUSTER.md) § Scheduler.

### `cls.script.add` / `cls.script.add.`

```
( metadata:MAP  script:STR -- id:STRING )
```

Metadata MUST include `name` and `schedule` (crontab-style).
Example:

```bund
dict
"name"     "hourly-cpu-report" set
"schedule" "0 * * * *"         set
"2 2 + println"
cls.script.add
```

### `cls.script.update` / `cls.script.update.`

```
( id:STR  metadata:MAP  script:STR -- nodata )
```

Full overwrite of metadata + body.

### `cls.script.delete` / `cls.script.delete.`

```
( id:STR -- nodata )
```

Replicated delete with tombstone.

### `cls.script.get` / `cls.script.get.`

```
( id:STR -- {id, script, metadata} | nodata )
```

Local-only fetch.

### `cls.scripts.list` / `cls.scripts.list.`

```
( -- [{id, metadata}, ...] )
```

Local-only enumeration of every stored script.

---

## 11. LLM — `cls.llm.*`

LLM coordination helpers backed by the v4/llm.* surface
([`../LLM.md`](../LLM.md)).  Every call routes through the
process-wide `ProviderManager`; in cluster mode the inference cache
and single-execution dedup layers kick in automatically.

### `cls.llm.complete` / `cls.llm.complete.`

```
( req:MAP -- response:MAP )
```

`req` keys:

| Key             | Type    | Notes                                                       |
|-----------------|---------|-------------------------------------------------------------|
| `prompt`        | STR     | Single-user-turn prompt (alternative to `messages`)         |
| `messages`      | LIST    | `[{role:"system|user|assistant", content:"..."}, ...]`     |
| `provider`      | STR     | `""` = use `llm.default`                                    |
| `model`         | STR     | `""` = use provider's `default_model`                       |
| `cache`         | BOOL    | `false` skips the inference cache                           |
| `options.temperature` | FLOAT |                                                          |
| `options.max_tokens` | INT  |                                                            |
| `options.top_p` | FLOAT   |                                                             |
| `options.seed`  | INT     | Ollama / OpenAI only                                        |
| `options.num_ctx` | INT   | Override auto-bucketed context window                       |

Response:

```text
{ response, provider, model, cache, dedup, prompt_chars,
  num_ctx, tokens_in, tokens_out, ms }
```

### `cls.llm.chat` / `cls.llm.chat.`

```
( req:MAP -- response:MAP )
```

Stateful chat turn — history is persisted in the docstore under
`chat_id`.  `req` accepts:

| Key        | Type | Notes                                                            |
|------------|------|------------------------------------------------------------------|
| `message`  | STR  | The new user turn (required)                                     |
| `chat_id`  | STR  | Omit on first turn — the response carries the new chat_id        |
| `duration` | STR  | When set, inline RAG over the docstore for the window            |
| `context`  | STR  | Pre-built RAG context (overrides `duration`)                     |
| `provider` | STR  |                                                                  |
| `model`    | STR  |                                                                  |

### `cls.llm.analyze` / `cls.llm.analyze.`

```
( req:MAP -- response:MAP )
```

RAG over a `ContextSource` kind + completion.  `req.kind` is one of
`aggregation`, `knn`, `rca`, `anomaly`, `templates`, `telemetry`,
`documents`, `supplied`.  See [`../LLM.md`](../LLM.md) § 3 for the
per-kind input schema.

```bund
dict
"kind"     "rca"             set
"duration" "1h"              set
"query"    "what is failing?" set
cls.llm.analyze
```

### `cls.llm.embed` / `cls.llm.embed.`

```
( req:MAP -- {vectors:LIST<LIST<FLOAT>>, dim:INT, ...} )
```

`req.texts` is a LIST of STRINGs; one embedding vector per input.

### `cls.llm.providers` / `cls.llm.providers.`

```
( -- {default:STR, providers:LIST<MAP>} )
```

Registered providers + capabilities + the configured default.

### `cls.llm.complete.async` / `cls.llm.complete.async.`

```
( req:MAP -- {job_id:STR, result_id:STR, kind:"complete", state:"pending"} )
```

Submit a completion for the background runner; poll
`v2/results.pull` with `result_id` for the eventual result.

### `cls.llm.analyze.async` / `cls.llm.analyze.async.`

Same as the sync `cls.llm.analyze`, async-submission variant.

### `cls.llm.jobs.list` / `cls.llm.jobs.list.`

```
( filter:MAP -- {jobs:LIST, count:INT} )
```

`filter.state` may be `"pending"` / `"running"` / `"done"` /
`"failed"` / `"cancelled"`.  `filter.limit` caps the list.

### `cls.llm.status` / `cls.llm.status.`

```
( job_id:STR|MAP -- {state, result?, error?, ...} )
```

Inspect one async job.  `job_id` may be a bare STRING or
`{"job_id":"..."}`.

### `cls.llm.cancel` / `cls.llm.cancel.`

```
( job_id:STR|MAP -- {ok:BOOL, job_id:STR} )
```

Cancel a pending or running job (idempotent on terminal states).

---

## 12. Introspection — `?cluster.meta` / `?llm.meta`

Every `cls.*` helper stashes a per-thread "meta" record before
returning.  Two read-only words surface that record:

### `?cluster.meta` / `?cluster.meta.`

```
( -- meta:MAP | nodata )
```

Reads the most-recent `cls.*` (non-LLM) call's outcome.  Two
shapes, depending on whether the last call was a read or a write:

**After a fan-out read:**

```json
{
  "enabled":        true,
  "peers_queried":  2,
  "peers_answered": 2,
  "partial":        false,
  "failed":         []
}
```

**After a replicated write:**

```json
{
  "enabled": true,
  "replication": {
    "peers_attempted":  2,
    "peers_succeeded":  2,
    "hints_queued":     0
  }
}
```

**After a local-only call** (`cls.doc.get.metadata`,
`cls.signal.get`, `cls.scripts.list`, `cls.script.get`, etc.) the
meta is cleared back to `nodata`.

**In standalone mode** every `cls.*` helper sets
`{"enabled": false}`.

### `?llm.meta` / `?llm.meta.`

```
( -- meta:MAP | nodata )
```

Same idea, but tracks the most-recent `cls.llm.*` call.  Carries
`{provider, model, cache, dedup, prompt_chars, num_ctx, tokens_in,
tokens_out, ms, kind?, n_rows?}` — the same introspection block the
response itself returns.

---

## 13. End-to-end recipe

Six families combined into one script — a watchdog that detects an
anomaly, summarises it, files a signal, and stores the result as a
document for later retrieval:

```bund
// 1. Pull recent fingerprints, find anomalies cluster-wide.
"15m" dict cls.anomaly.recent ! $anomalies

// 2. If there are any, summarise the top candidates with LLM analyze.
$anomalies "anomalies" $get $len 0 > {

  // RAG-grounded summary across the 15-minute window.
  dict
  "kind"     "anomaly"     set
  "duration" "15m"         set
  cls.llm.analyze ! $summary

  // 3. Emit a replicated signal pointing at the summary.
  "watchdog.anomaly"     // name
  "warn"                 // severity
  $now                   // unix seconds
  dict
    "summary"  $summary "response" $get    set
    "provider" $summary "provider" $get    set
    "model"    $summary "model"    $get    set
  cls.signal.emit ! $signal_id

  // 4. Persist the full LLM verdict in the docstore for later.
  dict
  "name"        "watchdog-anomaly-summary"  set
  "signal_id"   $signal_id                  set
  "internal_doc" false                      set
  $summary "response" $get
  cls.doc.add ! $doc_id

  // 5. Final stack value: the new doc UUID for the operator.
  $doc_id println

} if

// 6. Tail the meta cell so the operator can see what fanned out.
?cluster.meta.
```

The script behaves identically in standalone and cluster mode —
only the meta payload changes.  Adapt it to your alerting
preferences by swapping `cls.anomaly.recent` for `cls.rca`,
`cls.knn`, or `cls.denoise.recent`.

---

## 14. Quick reference

Every registered `cls.*` word, alphabetical.  Each name also has a
trailing-`.` workbench twin.

| Word                                | Family    | In → Out                                                          |
|-------------------------------------|-----------|-------------------------------------------------------------------|
| `cls.add`                           | add       | `MAP → STRING`                                                    |
| `cls.add.batch`                     | add       | `LIST<MAP> → LIST<STRING>`                                        |
| `cls.aggregation`                   | search    | `STR,STR → {observability,documents}`                             |
| `cls.anomaly.recent`                | analytics | `STR,MAP → {anomalies,...}`                                       |
| `cls.count`                         | inventory | `MAP → {count,local_count,...}`                                   |
| `cls.delete`                        | add       | `STR → nodata`                                                    |
| `cls.denoise.recent`                | analytics | `STR,MAP → {kept,removed,...}`                                    |
| `cls.doc.add`                       | docs      | `MAP,BIN|STR → STRING`                                            |
| `cls.doc.add.file`                  | docs      | `MAP,STR → STRING`                                                |
| `cls.doc.delete`                    | docs      | `STR → nodata`                                                    |
| `cls.doc.get.content`               | docs      | `STR → BIN`                                                       |
| `cls.doc.get.metadata`              | docs      | `STR → MAP | nodata`                                              |
| `cls.doc.reindex`                   | docs      | `→ INT`                                                           |
| `cls.doc.search`                    | docs      | `STR|MAP,INT → {results}`                                         |
| `cls.doc.search.json`               | docs      | `MAP,INT → {results}`                                             |
| `cls.doc.search.json.strings`       | docs      | `MAP,INT → {results:[STR]}`                                       |
| `cls.doc.search.strings`            | docs      | `STR|MAP,INT → {results:[STR]}`                                   |
| `cls.doc.sync`                      | docs      | `→ nodata`                                                        |
| `cls.doc.update.content`            | docs      | `STR,BIN|STR → nodata`                                            |
| `cls.doc.update.metadata`           | docs      | `STR,MAP → nodata`                                                |
| `cls.duplicates`                    | inventory | `MAP → {id → [ts]}`                                               |
| `cls.fingerprints.recent`           | inventory | `STR → {fingerprints}`                                            |
| `cls.fulltext`                      | search    | `STR,STR,INT → {results:[{id,score}]}`                            |
| `cls.fulltext.get`                  | search    | `STR,STR,INT → {results:[doc]}`                                   |
| `cls.fulltext.recent`               | search    | `STR,STR,INT → {results:[{id,ts,score}]}`                         |
| `cls.keys`                          | keys      | `STR → {keys}`                                                    |
| `cls.keys.all`                      | keys      | `STR,STR → {keys}`                                                |
| `cls.keys.get`                      | keys      | `STR,STR → {results}`                                             |
| `cls.knn`                           | analytics | `STR,MAP → analysis`                                              |
| `cls.llm.analyze`                   | llm       | `MAP → response`                                                  |
| `cls.llm.analyze.async`             | llm       | `MAP → {job_id,result_id,...}`                                    |
| `cls.llm.cancel`                    | llm       | `STR|MAP → {ok,job_id}`                                           |
| `cls.llm.chat`                      | llm       | `MAP → response`                                                  |
| `cls.llm.complete`                  | llm       | `MAP → response`                                                  |
| `cls.llm.complete.async`            | llm       | `MAP → {job_id,result_id,...}`                                    |
| `cls.llm.embed`                     | llm       | `MAP → {vectors,dim,...}`                                         |
| `cls.llm.jobs.list`                 | llm       | `MAP → {jobs,count}`                                              |
| `cls.llm.providers`                 | llm       | `→ {default,providers}`                                           |
| `cls.llm.status`                    | llm       | `STR|MAP → job status`                                            |
| `cls.primaries`                     | keys      | `MAP → {ids}`                                                     |
| `cls.primaries.explore`             | keys      | `STR → {results}`                                                 |
| `cls.primaries.explore.telemetry`   | keys      | `STR → {results}`                                                 |
| `cls.primaries.get`                 | keys      | `STR,STR → {results}`                                             |
| `cls.primaries.get.telemetry`       | keys      | `STR,STR → {results}`                                             |
| `cls.primary`                       | keys      | `STR → MAP`                                                       |
| `cls.rca`                           | analytics | `MAP → {clusters,ranked_causes}`                                  |
| `cls.rca.templates`                 | analytics | `MAP → {clusters,ranked_causes}`                                  |
| `cls.script.add`                    | scripts   | `MAP,STR → STRING`                                                |
| `cls.script.delete`                 | scripts   | `STR → nodata`                                                    |
| `cls.script.get`                    | scripts   | `STR → {id,script,metadata}`                                      |
| `cls.script.update`                 | scripts   | `STR,MAP,STR → nodata`                                            |
| `cls.scripts.list`                  | scripts   | `→ [{id,metadata}]`                                               |
| `cls.search`                        | search    | `*,STR → {results}`                                               |
| `cls.search.fts`                    | search    | `STR,STR → {results}`                                             |
| `cls.search.get`                    | search    | `*,STR,INT → {results}`                                           |
| `cls.secondaries`                   | keys      | `STR → {ids}`                                                     |
| `cls.secondary`                     | keys      | `STR → MAP`                                                       |
| `cls.signal.emit`                   | signals   | `STR,STR,INT,MAP → STRING`                                        |
| `cls.signal.get`                    | signals   | `STR → MAP | nodata`                                              |
| `cls.signal.update`                 | signals   | `STR,MAP → nodata`                                                |
| `cls.signals.query`                 | signals   | `STR,INT → {count,results}`                                       |
| `cls.signals.recent`                | signals   | `STR → {count,signals}`                                           |
| `cls.summary.lsa.query`             | analytics | `STR,MAP → {summary}`                                             |
| `cls.summary.lsa.recent`            | analytics | `STR,MAP → {summary}`                                             |
| `cls.summary.query`                 | analytics | `STR,MAP → {summary}`                                             |
| `cls.summary.recent`                | analytics | `STR,MAP → {summary}`                                             |
| `cls.textrank.templates`            | analytics | `STR,MAP → {summary}`                                             |
| `cls.timeline`                      | analytics | `→ {min_ts,max_ts}`                                               |
| `cls.topics`                        | analytics | `MAP → {topic,terms}`                                             |
| `cls.topics.all`                    | analytics | `MAP → {topics}`                                                  |
| `cls.tpl.add`                       | templates | `MAP,BIN|STR → STRING`                                            |
| `cls.tpl.delete`                    | templates | `STR → nodata`                                                    |
| `cls.tpl.get`                       | templates | `STR → {id,metadata,body}`                                        |
| `cls.tpl.list`                      | templates | `STR → {templates}`                                               |
| `cls.tpl.reindex`                   | templates | `STR → INT`                                                       |
| `cls.tpl.search`                    | templates | `STR,STR,INT → {results}`                                         |
| `cls.tpl.template.by.id`            | templates | `STR → {template}`                                                |
| `cls.tpl.templates.by.timestamp`    | templates | `INT,INT → {templates}`                                           |
| `cls.tpl.templates.recent`          | templates | `STR → {templates}`                                               |
| `cls.tpl.update.body`               | templates | `STR,BIN|STR → nodata`                                            |
| `cls.tpl.update.metadata`           | templates | `STR,MAP → nodata`                                                |
| `cls.trends`                        | analytics | `STR,STR → {min,max,mean,...}`                                    |
| `cls.update`                        | add       | `STR,MAP → STRING`                                                |
| `?cluster.meta`                     | meta      | `→ MAP | nodata`                                                  |
| `?llm.meta`                         | meta      | `→ MAP | nodata`                                                  |

Every word above also has a trailing-`.` workbench variant with the
same input / output signature, just reading from / writing to the
workbench instead of the main stack.

---

## See also

- [`BDS.md`](BDS.md) — the **local-only** `db.*` / `doc.*` family.
  `cls.*` is the cluster-aware mirror; choose `cls.*` when you want
  fan-out / replication, `db.*` / `doc.*` when you specifically
  want local-only.
- [`SYNTAX_AND_VM.md`](SYNTAX_AND_VM.md) — BUND language reference.
- [`../CLUSTER.md`](../CLUSTER.md) — cluster mode + replication
  semantics.
- [`../LLM.md`](../LLM.md) — LLM surface incl. `v3/help` and
  `v2/to.bund`.
- [`examples/cluster/`](../../examples/cluster/) — eight runnable
  end-to-end Bund scripts demonstrating the families above.
