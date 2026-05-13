# BDS — BUND Database & Document Store Words

Reference for every BUND word provided by `src/vm/stdlib/db`.  Words
are split into two groups:

- **Shard DB** (`db.*`) — telemetry/event storage: vector search,
  full-text search, and aggregated cross-store search over
  time-windowed shards.
- **Document Store** (`doc.*`) — persistent document store: add,
  update, delete, retrieve, and search documents by text, JSON, or
  pre-computed embedding vectors.

> **Looking for cluster-aware versions?**  Every word in this
> reference has a `cls.*` twin that transparently fans out across
> peers in cluster mode and degrades to a local call in standalone
> mode.  See [BUND_BDS.md](BUND_BDS.md).  Choose `db.*` / `doc.*`
> when you specifically want local-only; `cls.*` otherwise.

---

## Conventions

**Stack-effect notation:** `( before -- after )` where the top of
the stack is on the right.  `W:x` denotes a value on the
**workbench** instead of the stack.  A trailing `.` on a word name
means the word reads from and writes to the **workbench** instead
of the main stack — stack and workbench variants always co-exist
with identical semantics.

**Map construction.**  Bund has no map literal — `{ … }` is a
**lambda**, `[ … ]` is a **list**, and `( … )` is a context.  Maps
are built imperatively with `dict` + `set`:

```bund
dict
"key1" "value1" set
"key2" 42       set
"key3" true     set
// stack top: { "key1": "value1", "key2": 42, "key3": true }
```

`set` has the stack effect `( map key value -- map' )` — pops the
value (top), then the key, then the map; pushes the updated map.

**List construction.**  `[ 1 2 "x" true ]` builds a heterogeneous
list.  At least one element is required — there is no empty-list
literal.  Use `list.new` if you need an empty list.

**JSON conversion.**  Every word that takes a Map (e.g. `db.add`,
`doc.add`, `doc.search.json`) calls `dynamic_to_json` internally,
so the same script works whether you build the value with
`dict … set` or with `convert.to_dict` over a JSON string.

---

## Table of contents

1. [Shard DB — Ingest](#1-shard-db--ingest)
2. [Shard DB — Search](#2-shard-db--search)
3. [Shard DB — Aggregation Search](#3-shard-db--aggregation-search)
4. [Shard DB — Sync](#4-shard-db--sync)
5. [Document Store — Add](#5-document-store--add)
6. [Document Store — Update & Store Vectors](#6-document-store--update--store-vectors)
7. [Document Store — Delete](#7-document-store--delete)
8. [Document Store — Retrieve](#8-document-store--retrieve)
9. [Document Store — Search (full results)](#9-document-store--search-full-results)
10. [Document Store — Search (fingerprint strings)](#10-document-store--search-fingerprint-strings)
11. [Document Store — Sync & Reindex](#11-document-store--sync--reindex)
12. [End-to-end recipes](#12-end-to-end-recipes)
13. [Quick reference](#13-quick-reference)
14. [Operator sandbox](#14-operator-sandbox)

---

## 1. Shard DB — Ingest

### `db.add` / `db.add.`

```
( doc:MAP -- id:STRING )
( W:doc:MAP -- W:id:STRING )
```

Converts `doc` to JSON via `dynamic_to_json` and stores it in the
time-series shard DB.  The shard is chosen by `doc["timestamp"]`
when present, else by wall-clock now.  Pushes the assigned record
UUIDv7 as a STRING.

```bund
dict
"key"       "nginx.error"           set
"timestamp" 1700000000              set
"host"      "web01"                 set
"level"     "error"                 set
"msg"       "connection refused"   set
db.add
// stack: "019e21f6-7a3e-..."
```

---

## 2. Shard DB — Search

### `db.search` / `db.search.`

```
( query:any  duration:STRING -- results:LIST )
( W:query:any  W:duration:STRING -- W:results:LIST )
```

Vector similarity search over shards within the trailing time
window.  `query` may be a STRING or a MAP — both are converted to
JSON via `dynamic_to_json`, fingerprinted, and embedded with the
shared AllMiniLML6V2 model.  `duration` is a humantime string such
as `"1h"`, `"30min"`, `"7days"`.  Returns a LIST of result MAPs
ranked by descending cosine similarity.

```bund
// String query.
"login failures" "1h" db.search
// stack: [ { "_score": 0.91, "key": "auth.fail", ... }, ... ]

// Structured (Map) query — fingerprinted before embedding.
dict
"level" "error" set
"host"  "web01" set
"1h" db.search
```

### `db.fulltext` / `db.fulltext.`

```
( query:STRING  duration:STRING -- results:LIST )
( W:query:STRING  W:duration:STRING -- W:results:LIST )
```

Tantivy BM25 full-text search over shards within the window.
`query` is a plain search string; Tantivy's query language
operators (`AND`, `OR`, `"phrase"`, `field:term`) work as usual.
Returns a LIST of result MAPs.

```bund
"nginx upstream timeout" "6h" db.fulltext
// stack: [ { ..., "_score": 4.21 }, ... ]
```

---

## 3. Shard DB — Aggregation Search

### `db.aggregation.search` / `db.aggregation.search.`

```
( query:STRING  duration:STRING -- result:MAP )
( W:query:STRING  W:duration:STRING -- W:result:MAP )
```

Runs a telemetry vector search **and** a document-store semantic
search **concurrently** (via Rayon) using the same plain-text
`query`, then merges the result sets into a single MAP with two
keys:

| Key | Type | Contents |
|---|---|---|
| `"observability"` | LIST<MAP> | Telemetry records from the shard DB, vector-ranked by `_score` descending.  Each record carries `_score` and an embedded `secondaries` array. |
| `"documents"`     | LIST<MAP> | Document-store hits from the semantic search (up to 10).  Each hit carries `id`, `metadata`, `document`, `score`. |

`duration` is the lookback window for the telemetry side only
(`"1h"`, `"30min"`, `"7days"`).  The document-store search is
global — it is **not** filtered by time.

The query is fingerprinted and embedded once with the shared
AllMiniLML6V2 model before being passed to the HNSW indexes on
both sides.  If either search errors the word fails and nothing is
pushed.

```bund
"connection pool exhaustion" "1h" db.aggregation.search
// stack: {
//   "observability": [ { "_score": 0.92, "host": "web01", ... }, ... ],
//   "documents":     [ { "id": "d2b4...", "score": 0.87, ... }, ... ]
// }
```

Workbench variant — useful in pipelines that pass results directly
to other `.`-suffixed words:

```bund
"nginx upstream timeout" "6h" db.aggregation.search.
// workbench top: result MAP
```

---

## 4. Shard DB — Sync

### `db.sync`

```
( -- true:BOOL )
```

Flushes pending shard-DB writes to disk (DuckDB CHECKPOINT across
every open shard).  Pushes `true` on success; errors on failure.
**No workbench variant** — sync is a process-wide side effect, not
something you'd want to chain through the workbench.

```bund
db.sync drop   // flush and discard the confirmation flag
```

---

## 5. Document Store — Add

### `doc.add` / `doc.add.`

```
( metadata:MAP  content:STRING -- id:STRING )
( W:metadata:MAP  W:content:STRING -- W:id:STRING )
```

Adds a new document.  `metadata` is a MAP serialised to JSON;
`content` is the UTF-8 body.  The store automatically generates two
embeddings (`<uuid>:meta` and `<uuid>:content`) into the shared
HNSW index.  Pushes the new document UUIDv7 as a STRING.

```bund
dict
"title"   "Release notes"     set
"version" "1.2"               set
"tags"    [ "release" "v1" ]  set
"This release fixes the connection-pool exhaustion bug."
doc.add
// stack: "d2b4a1f0-..."
```

### `doc.add.file` / `doc.add.file.`

```
( path:STRING  name:STRING  slice:INT  overlap:FLOAT -- id:STRING )
( W:path:STRING  W:name:STRING  W:slice:INT  W:overlap:FLOAT -- W:id:STRING )
```

Reads a file from `path`, slices it semantically (paragraph →
sentence → word boundaries) into chunks of approximately `slice`
characters with `overlap`-fraction overlap, embeds each chunk, and
stores the document.  `name` becomes `metadata.name`.  Pushes the
**root document UUID** — child chunks live under their own UUIDs
listed in `metadata.chunks`.

| Argument  | Type   | Meaning                                                     |
|-----------|--------|-------------------------------------------------------------|
| `path`    | STRING | Filesystem path to the file (must be UTF-8 text)            |
| `name`    | STRING | Document name stored in metadata                            |
| `slice`   | INT    | Chunk size in characters (e.g. 512, 1024)                   |
| `overlap` | FLOAT  | Fractional overlap between chunks (0.0 = none, 0.5 = half)  |

```bund
"/var/log/app.log" "app.log" 1024 0.2 doc.add.file
// stack: "d2b4a1f0-..."  (root document id)
```

See [`DOCUMENTSENGINE.md`](../DOCUMENTSENGINE.md) for the chunking
algorithm.

### `doc.add.vec` / `doc.add.vec.`

```
( metadata:MAP  content:STRING  meta_vec:LIST  content_vec:LIST -- id:STRING )
( W:metadata:MAP  W:content:STRING  W:meta_vec:LIST  W:content_vec:LIST -- W:id:STRING )
```

Adds a document with **pre-computed** embedding vectors, bypassing
the built-in embedder.  `meta_vec` and `content_vec` are LISTs of
FLOATs (typically 384-dim with the default AllMiniLML6V2; whatever
dim the operator's embedder produces).  Useful when embeddings are
computed off-node in a different pipeline stage.

```bund
dict
"source" "api"                     set
"kind"   "rate-limit-error"        set
"Error: rate limit exceeded for tenant 42"
[ 0.12 0.84 0.03 0.77 0.21 ]      // meta_vec (full size in real use)
[ 0.45 0.19 0.62 0.04 0.88 ]      // content_vec
doc.add.vec
```

---

## 6. Document Store — Update & Store Vectors

### `doc.update.metadata` / `doc.update.metadata.`

```
( id:STRING  metadata:MAP -- true:BOOL )
( W:id:STRING  W:metadata:MAP -- W:true:BOOL )
```

Replaces the metadata JSON for document `id` with `metadata`
(full overwrite, **not** merge).  The metadata HNSW vector is
**not** automatically refreshed — call `doc.reindex` after bulk
metadata edits or use `doc.store.meta.vec` to update both at once.
Pushes `true` on success.

```bund
"d2b4a1f0-..."
dict
"title"    "Release notes v2"  set
"version"  "1.2"                set
"reviewed" true                 set
doc.update.metadata drop
```

### `doc.update.content` / `doc.update.content.`

```
( id:STRING  content:STRING -- true:BOOL )
( W:id:STRING  W:content:STRING -- W:true:BOOL )
```

Replaces the stored content bytes for document `id`.  The content
HNSW vector is **not** automatically rebuilt — call `doc.reindex`
afterwards if search results must reflect the new content.

```bund
"d2b4a1f0-..." "Updated body text after editorial review." doc.update.content drop
doc.reindex println    // flush the index then print how many docs were re-embedded
```

### `doc.store.meta.vec` / `doc.store.meta.vec.`

```
( id:STRING  meta_vec:LIST  metadata:MAP -- true:BOOL )
( W:id:STRING  W:meta_vec:LIST  W:metadata:MAP -- W:true:BOOL )
```

Stores a pre-computed metadata embedding vector **together with**
new metadata for document `id`.  Useful for refreshing both at once
when the embedding pipeline lives outside the bdsnode (e.g. when
piping through a custom OpenAI-embeddings step).

```bund
"d2b4a1f0-..."
[ 0.12 0.84 0.03 0.77 ]
dict
"title"   "Updated"          set
"version" "1.3"              set
doc.store.meta.vec drop
```

### `doc.store.content.vec` / `doc.store.content.vec.`

```
( id:STRING  content_vec:LIST -- true:BOOL )
( W:id:STRING  W:content_vec:LIST -- W:true:BOOL )
```

Stores a pre-computed content embedding vector for document `id`
without touching the content bytes or metadata.  Pair with
`doc.update.content` when refreshing a doc with externally-embedded
text.

```bund
"d2b4a1f0-..." [ 0.03 0.77 0.45 0.19 ] doc.store.content.vec drop
```

---

## 7. Document Store — Delete

### `doc.delete` / `doc.delete.`

```
( id:STRING -- true:BOOL )
( W:id:STRING -- W:true:BOOL )
```

Permanently removes document `id` from all three sub-stores
(metadata JSON, content blob, HNSW vector slots `<id>:meta` and
`<id>:content`).  The HNSW index keeps a tombstone slot until the
next `doc.reindex` — searches won't surface deleted docs in the
meantime, but the index file doesn't shrink on disk until then.

```bund
"d2b4a1f0-..." doc.delete drop
```

---

## 8. Document Store — Retrieve

### `doc.get.metadata` / `doc.get.metadata.`

```
( id:STRING -- MAP | null )
( W:id:STRING -- W:MAP | W:null )
```

Retrieves the metadata MAP for document `id`.  Pushes `null`
(`NODATA`) if the document does not exist — guard with `?type`
when uncertain.

```bund
"d2b4a1f0-..." doc.get.metadata
// stack: { "title": "Release notes", "version": "1.2", ... }
```

### `doc.get.content` / `doc.get.content.`

```
( id:STRING -- STRING | null )
( W:id:STRING -- W:STRING | W:null )
```

Retrieves the raw content bytes for document `id`, decoded as
UTF-8, and pushes them as a STRING.  Returns `null` if the
document does not exist.

```bund
"d2b4a1f0-..." doc.get.content println
```

---

## 9. Document Store — Search (full results)

All search words in this section return a **LIST of MAPs** — each
MAP carries `id`, `metadata`, `document`, and `score`.

### `doc.search` / `doc.search.`

```
( query:STRING  limit:INT -- results:LIST )
( W:query:STRING  W:limit:INT -- W:results:LIST )
```

Embeds `query` with the built-in text embedder and performs HNSW
search over document **content** vectors.  Returns up to `limit`
result MAPs ranked by descending cosine similarity.

```bund
"connection pool exhaustion" 5 doc.search
// stack: [ { "id": "...", "metadata": {...}, "document": "...", "score": 0.87 }, ... ]
```

### `doc.search.json` / `doc.search.json.`

```
( query:MAP  limit:INT -- results:LIST )
( W:query:MAP  W:limit:INT -- W:results:LIST )
```

Converts the `query` MAP to a `json_fingerprint` string, embeds
it, and performs HNSW search over document **metadata** vectors.
Returns up to `limit` result MAPs.

```bund
dict
"level" "error" set
"host"  "web01" set
10 doc.search.json
```

### `doc.search.vec` / `doc.search.vec.`

```
( query_vec:LIST  limit:INT -- results:LIST )
( W:query_vec:LIST  W:limit:INT -- W:results:LIST )
```

Performs HNSW search using a **pre-computed** embedding vector.
`query_vec` must be a LIST of FLOATs with the same dimensionality
as the docstore's index (default 384 for AllMiniLML6V2).

```bund
[ 0.03 0.77 0.45 0.19 0.88 ] 5 doc.search.vec
```

---

## 10. Document Store — Search (fingerprint strings)

These words return a **LIST of STRINGs** — the raw
`json_fingerprint` string for each matching document — instead of
full result MAPs.  Useful for lightweight lookups, deduplication
pipelines, or as RAG context for an LLM call.

### `doc.search.strings` / `doc.search.strings.`

```
( query:STRING  limit:INT -- fingerprints:LIST )
( W:query:STRING  W:limit:INT -- W:fingerprints:LIST )
```

Text query → HNSW search over content vectors → list of
fingerprint strings.

```bund
"nginx upstream timeout" 10 doc.search.strings
```

### `doc.search.json.strings` / `doc.search.json.strings.`

```
( query:MAP  limit:INT -- fingerprints:LIST )
( W:query:MAP  W:limit:INT -- W:fingerprints:LIST )
```

JSON MAP query → fingerprint → HNSW search over metadata vectors
→ list of fingerprint strings.

```bund
dict
"service" "auth" set
5 doc.search.json.strings
```

### `doc.search.vec.strings` / `doc.search.vec.strings.`

```
( query_vec:LIST  limit:INT -- fingerprints:LIST )
( W:query_vec:LIST  W:limit:INT -- W:fingerprints:LIST )
```

Pre-computed vector → HNSW search → list of fingerprint strings.

```bund
[ 0.12 0.84 0.03 0.77 0.21 ] 5 doc.search.vec.strings
```

---

## 11. Document Store — Sync & Reindex

### `doc.sync`

```
( -- true:BOOL )
```

Flushes the document store's HNSW index to disk.  Pushes `true` on
success.  Call after bulk ingestion to ensure durability.  **No
workbench variant.**

```bund
doc.sync drop
```

### `doc.reindex` / `doc.reindex.`

```
( -- count:INT )
( W: -- W:count:INT )
```

Rebuilds the HNSW index from the persisted metadata + content
vectors (drops tombstones, compacts on-disk).  Returns the number
of documents re-indexed.  Use after `doc.update.content`,
`doc.delete`, bulk `doc.store.*.vec` updates, or an unclean
shutdown.

```bund
doc.reindex println
// stdout: 1247
```

---

## 12. End-to-end recipes

### Ingest → search → render

```bund
// 1. Ingest a record into the shard DB.
dict
"key"       "kernel.oom"                                              set
"timestamp" 1700000000                                                set
"host"      "compute-07"                                              set
"msg"       "Out of memory: Killed process 12480 (java) total-vm: …"  set
db.add

// 2. Ingest a runbook into the docstore.
dict
"name"    "oom-runbook.md"   set
"tags"    [ "oom" "kernel" ] set
"When the JVM is OOM-killed, capture a heap dump from /var/crash …"
doc.add

// 3. Run a combined search — telemetry + docs at once.
"out of memory java"  "1h"  db.aggregation.search

// 4. Pretty-print the result.
.   // move to workbench so eval returns it as the response
```

### Pre-computed embeddings pipeline

```bund
// External pipeline produced both vectors.  Store them along with
// the new metadata in one shot.
"d2b4a1f0-..."
[ 0.03 0.41 0.77 0.12 0.55 0.08 ]    // meta_vec — 384-dim in real use
dict
"title"    "Hot-patch advisory"  set
"severity" "high"                set
doc.store.meta.vec drop

// And the content vector independently.
"d2b4a1f0-..."
[ 0.62 0.04 0.88 0.19 0.45 0.31 ]
doc.store.content.vec drop

// Make the new vectors searchable.
doc.reindex println
```

---

## 13. Quick reference

| Word                         | Stack (`before -- after`)                      | Description                                                          |
|------------------------------|------------------------------------------------|----------------------------------------------------------------------|
| `db.add`                     | `( doc -- id )`                                | Ingest a document into the shard DB                                  |
| `db.search`                  | `( query duration -- results )`                | Vector search over time window                                       |
| `db.fulltext`                | `( query duration -- results )`                | Tantivy BM25 full-text search over time window                       |
| `db.aggregation.search`      | `( query duration -- result )`                 | Parallel telemetry + docstore search, merged MAP                     |
| `db.sync`                    | `( -- true )`                                  | Flush shard DB to disk                                               |
| `doc.add`                    | `( metadata content -- id )`                   | Add document, auto-embed                                             |
| `doc.add.file`               | `( path name slice overlap -- id )`            | Add document from file, chunk + auto-embed                           |
| `doc.add.vec`                | `( metadata content meta_vec content_vec -- id )` | Add document with pre-computed vectors                            |
| `doc.update.metadata`        | `( id metadata -- true )`                      | Replace metadata (full overwrite)                                    |
| `doc.update.content`         | `( id content -- true )`                       | Replace content bytes (does NOT re-embed)                            |
| `doc.store.meta.vec`         | `( id meta_vec metadata -- true )`             | Store metadata embedding + metadata together                         |
| `doc.store.content.vec`      | `( id content_vec -- true )`                   | Store content embedding only                                         |
| `doc.delete`                 | `( id -- true )`                               | Remove from all sub-stores                                           |
| `doc.get.metadata`           | `( id -- MAP \| null )`                        | Fetch metadata MAP                                                   |
| `doc.get.content`            | `( id -- STRING \| null )`                     | Fetch content as UTF-8 string                                        |
| `doc.search`                 | `( query limit -- results )`                   | Text → content vector search → MAPs                                  |
| `doc.search.json`            | `( query limit -- results )`                   | MAP → metadata vector search → MAPs                                  |
| `doc.search.vec`             | `( vec limit -- results )`                     | Pre-computed vector search → MAPs                                    |
| `doc.search.strings`         | `( query limit -- fingerprints )`              | Text → content vector search → strings                               |
| `doc.search.json.strings`    | `( query limit -- fingerprints )`              | MAP → metadata vector search → strings                               |
| `doc.search.vec.strings`     | `( vec limit -- fingerprints )`                | Pre-computed vector search → strings                                 |
| `doc.sync`                   | `( -- true )`                                  | Flush HNSW index to disk                                             |
| `doc.reindex`                | `( -- count )`                                 | Rebuild HNSW index from persisted vectors                            |

Every word except `db.sync` and `doc.sync` has a `.`-suffixed
workbench variant with identical semantics — the variant reads its
inputs from / writes its result to the workbench instead of the
main stack.

---

## 14. Operator sandbox

All write-side words above (`db.add`, `doc.add*`, `doc.update.*`,
`doc.delete`, `doc.reindex`, `doc.sync`, `db.sync`, plus their
workbench variants and the `doc.store.*` family) belong to the
`local_db_write` sandbox category.  Operators can disable them on
a per-node basis via `bds.hjson`:

```hjson
bund: {
  disabled_categories: ["local_db_write"]
}
```

The cluster-replicated equivalents (`cls.add`, `cls.doc.*`,
`cls.tpl.*`, `cls.signal.*`, `cls.script.*` and their workbench
variants) live in the `cluster_admin` category — disable that one
to make a node read-only from a BUND-script point of view.

See [`../BDSCONFIG.md`](../BDSCONFIG.md) § 4.1 and
[`BASIC_LIBRARY.md`](BASIC_LIBRARY.md) § 23 for the full word-to-
category mapping and recommended deployment profiles.
