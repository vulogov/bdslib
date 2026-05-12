# LLM — `v4/llm.*` cluster-aware inference surface

bdslib ships a full LLM integration layer on top of the v3/* cluster
primitives.  The same provider abstraction, RAG pipeline, replicated
cache, cluster-wide dedup, and async job queue power three independent
user-facing surfaces:

| Surface  | Driver                                                                                |
|----------|---------------------------------------------------------------------------------------|
| Web UI   | bdsweb `/chat` (with provider picker), `/admin/llm` (providers + cache + jobs admin)  |
| Shell    | `bdscmd llm <subcommand>` ([§ 7](#7--bdscmd-llm-subcommand))                          |
| Scripts  | `cls.llm.*` Bund words ([§ 8](#8--bund-clsllm-words))                                 |

This document is the canonical reference.  Specifics that already live
elsewhere are cross-linked rather than duplicated:

- Per-method RPC schemas → [`jsonrpc_api/README.md`](jsonrpc_api/README.md)
  table + individual `v4_llm_*.md` files
- Cluster gossip / replication / anti-entropy mechanics →
  [`CLUSTER.md`](CLUSTER.md) and [`CLUSTER_DETAILS.md`](CLUSTER_DETAILS.md)
- On-disk layout of the three new stores → [`DATABASE.md`](DATABASE.md)
  § 7 (`<dbpath>/llm/` and `<dbpath>/network/inference_log.duckdb`)

---

## Table of contents

1. [Architecture overview](#1--architecture-overview)
2. [Configuration (`llm.*` in bds.hjson)](#2--configuration)
3. [Provider layer](#3--provider-layer)
4. [Inference cache (replicated)](#4--inference-cache)
5. [Cluster-wide dedup (single-execution)](#5--cluster-wide-dedup)
6. [Async jobs + RESULTS](#6--async-jobs)
7. [`bdscmd llm` subcommand](#7--bdscmd-llm-subcommand)
8. [Bund `cls.llm.*` words](#8--bund-clsllm-words)
9. [bdsweb integration](#9--bdsweb-integration)
10. [JSON-RPC surface (`v4/llm.*` + `v2/llm.*`)](#10--json-rpc-surface)
11. [Diagnostics — `?llm.meta` + log lines](#11--diagnostics)
12. [Operational gotchas](#12--operational-gotchas)

---

## 1 · Architecture overview

```
                ┌─────────────────────────────────────────────┐
                │       v4/llm.*  (HMAC-signed RPC)           │
                │  complete · chat · analyze · embed          │
                │  providers.list                             │
                │  complete_async · analyze_async             │
                │  jobs.list · jobs.status · jobs.cancel      │
                │  cache.stats · cache.purge                  │
                └─────────────────────┬───────────────────────┘
                                      │
                  ┌───────────────────┼───────────────────┐
                  ▼                   ▼                   ▼
        ┌──────────────────┐  ┌──────────────┐  ┌─────────────────┐
        │ vm::api::llm     │  │ JobQueue +   │  │ Admin / stats   │
        │ helpers (sync)   │  │ runner       │  │ direct passes   │
        └────────┬─────────┘  └──────┬───────┘  └─────────────────┘
                 │                   │
   ┌─────────────┼─────────────┐     │
   ▼             ▼             ▼     ▼
 cache       dedup         provider  ResultQueue
 lookup      lease         call      push
   │           │             │         │
   ▼           ▼             ▼         ▼
inference   inference     Ollama /   v2/results.{push,pull}
cache       log           OpenAI /   (same path scripts use)
(replicated) (per-node)   Anthropic
```

Six phases ship the layers:

| Phase | What lands                                                                  |
|------:|-----------------------------------------------------------------------------|
| 0     | `Provider` trait + three impls (Ollama, Anthropic, OpenAI) + `ProviderManager` |
| 1     | `vm::api::llm` sync helpers + `v4/llm.{complete,chat,analyze,embed,providers.list}` + `cls.llm.*` + `?llm.meta` |
| 2     | `src/llm/context.rs` `ContextSource` pipeline (RAG) + `v4/llm.analyze` |
| 3     | Replicated `InferenceCache` + cache fan-out + AE hook + `v4/llm.cache.*` |
| 4     | Cluster-wide single-execution dedup via `InferenceLog` + `v2/llm.last_executed` |
| 5     | Local `JobQueue` + background runner + `v4/llm.{*_async,jobs.*}` |
| 6     | bdsweb integration — `/chat` provider picker + `/admin/llm` page; `bdscmd llm` |

Every helper goes through the existing `vm::api::dispatch` layer, so
standalone vs cluster mode is transparent — the same code runs on a
single-node lab box and a 3-node production cluster.

---

## 2 · Configuration

The `llm` block in `bds.hjson`:

```hjson
llm: {
  // Optional explicit default.  When unset, falls back to the first
  // successfully registered provider.
  default: "ollama"

  providers: {
    ollama: {
      url:           "http://127.0.0.1:11434"
      default_model: "llama3.2"
    }
    anthropic: {
      base_url:      "https://api.anthropic.com"  // optional
      api_key_env:   "ANTHROPIC_API_KEY"           // env var name (NOT the key)
      default_model: "claude-sonnet-4-5"
    }
    openai: {
      base_url:      "https://api.openai.com"
      api_key_env:   "OPENAI_API_KEY"
      default_model: "gpt-4o-mini"
    }
  }

  // Inference cache.  Stored replicated across the cluster like
  // docs/signals/scripts/users.  Disable per-call with `cache: false`
  // in the request body; temperature > 0 also skips caching by default.
  cache: {
    enabled:   true
    ttl_secs:  86400          // 24h; 0 = never expires
  }

  // Cluster-wide single-execution dedup.
  dedup: {
    enabled:        true
    window_secs:    300       // a recent done/failed still short-circuits within this window
    wait_max_secs:  30        // sync caller poll budget when a peer is mid-flight
  }

  // Async job runner.
  runner: {
    enabled:            true
    poll_interval_secs: 1
    max_concurrency:    2
  }
}
```

Providers requiring an API key read the secret from the env var named
in `api_key_env` — never from `bds.hjson` itself, so config files can
be checked in without leaking secrets.  An unset env var causes the
provider to be **logged and skipped** rather than failing node startup.

`cluster.full_replication_stores` must include `"llm_cache"` (and
`"users"`) for the cache to replicate.  The library default already
includes both; setups that override the list explicitly must add it
back.

---

## 3 · Provider layer

| Provider   | Chat | Embed | Notes                                                        |
|------------|------|-------|--------------------------------------------------------------|
| Ollama     | ✓    | ✓     | `/api/chat` (non-streaming) + `/api/embed`; honours `num_ctx` |
| Anthropic  | ✓    | —     | `/v1/messages` with system-prompt lift; `seed` field dropped |
| OpenAI     | ✓    | ✓     | `/v1/chat/completions` + `/v1/embeddings`                    |

All providers implement `bdslib::llm::providers::Provider` (async via
`#[async_trait]`).  The `ProviderManager` (lifecycle = process-wide
`OnceLock`) resolves them by name; callers pass `provider: "ollama"`
in a request or omit the field to use the configured default.

**Adding a provider** — implement `Provider`, register in
`ProviderManager::from_config`.  No schema changes needed in
v4/llm.*; the request body is provider-agnostic.

---

## 4 · Inference cache

**Storage** — DuckDB at `<dbpath>/llm/cache.duckdb`.  Schema:

```sql
CREATE TABLE inference_cache (
    id            TEXT PRIMARY KEY,        -- UUIDv7 (for anti-entropy)
    cache_key     TEXT UNIQUE NOT NULL,    -- sha256 hex of canonical request
    provider      TEXT NOT NULL,
    model         TEXT NOT NULL,
    kind          TEXT NOT NULL,           -- "complete" | "chat" | "analyze:rca" | ...
    request_json  TEXT NOT NULL,           -- REDACTED canonical request
    response_json TEXT NOT NULL,           -- {text, tokens_in?, tokens_out?, finish_reason?}
    source_meta   TEXT,                    -- ContextSource snapshot or NULL
    created_at    BIGINT NOT NULL,
    expires_at    BIGINT NOT NULL,         -- 0 = never expires
    updated_at    BIGINT NOT NULL,         -- LWW key for AE
    hits          BIGINT NOT NULL DEFAULT 0
);
```

**Cache key** — sha256 hex over a sorted-keys canonical JSON of:

- `kind` (`"complete"` | `"chat"` | `"analyze:<sub_kind>"`)
- `provider` + `model`
- For `complete`: messages array + sorted options
- For `analyze`: sorted fingerprint list + query + prompt_template +
  system_prompt + options

Fields with names matching `api_key`, `_hmac`, `authorization`, or
`secret` (case-insensitive) are **redacted** to `"***"` before the
hash is computed.  Rotating an API key therefore does NOT invalidate
the cache.

**Replication** — every successful local `cache_store` fans out to
every Alive peer via `v2/llm.cache.put`.  Anti-entropy sweeps the
store under the name `"llm_cache"` and pulls missing rows via
`v2/llm.cache.get.by_id`.

**Per-call opt-out** — set `cache: false` in any v4/llm.* request.

**Auto-disable for non-determinism** — `temperature > 0` skips
caching automatically (reasoning: two identical requests with
temperature 0.7 produce different answers; caching one would mask
the variability).  Cache disposition surfaces in every response:

| `cache` field              | Meaning                                                |
|----------------------------|--------------------------------------------------------|
| `"hit"`                    | Returned from the cache; `ms: 0`, no provider call     |
| `"miss"`                   | Provider called, response written to cache             |
| `"disabled"`               | Global cache off or manager unset                      |
| `"disabled:opt-out"`       | Per-call `cache: false`                                |
| `"disabled:temperature"`   | `temperature > 0`                                      |
| `"disabled:chat"`          | Chat turns are never cacheable (history never repeats) |

---

## 5 · Cluster-wide dedup

**Storage** — DuckDB at `<dbpath>/network/inference_log.duckdb`.  Per-
node, not replicated — cross-node visibility is via `v2/llm.last_executed`
fan-out (same shape as `v2/scheduler.last_seen`).

```sql
CREATE TABLE inference_log (
    cache_key    TEXT NOT NULL,    -- same key as inference_cache
    started_at   BIGINT NOT NULL,
    finished_at  BIGINT,
    node_id      TEXT NOT NULL,
    state        TEXT NOT NULL     -- "running" | "done" | "failed"
);
```

**Lease flow** in `vm::api::llm::complete` / `analyze` on a cache miss:

1. Compute cache_key.
2. `recent_within(cache_key, window_secs)` on the local log.
3. If nothing locally, fan `v2/llm.last_executed` to every Alive peer.
4. Outcome:
   - `Acquired(lease)`   — provider call + `release_done()`/`release_failed()`
   - `SkipRunning`       — peer is mid-flight; poll cache for `wait_max_secs`
   - `SkipDone`          — recently completed; poll cache for `wait_max_secs`
   - `Disabled`          — standalone mode or `llm.dedup.enabled=false`
5. On `Skip*`, after timeout, **fall through and run the inference
   anyway** (sync callers don't want to wait forever).

`InferenceLease::Drop` records `failed` when not explicitly released,
so a panic mid-call never strands a `running` row.

The `dedup` field on every response: `"ran"` | `"waited"` |
`"skipped:done"` | `"disabled"`.  Cache hits don't carry it (no dedup
decision was made — cache short-circuited first).

---

## 6 · Async jobs

**Local storage** — DuckDB at `<dbpath>/llm/jobs.duckdb`:

```sql
CREATE TABLE llm_jobs (
    job_id       TEXT PRIMARY KEY,  -- UUIDv7
    result_id    TEXT NOT NULL,     -- UUIDv7 for v2/results.pull
    kind         TEXT NOT NULL,     -- "complete" | "analyze:<sub_kind>"
    request_json TEXT NOT NULL,
    state        TEXT NOT NULL,     -- pending | running | done | failed | cancelled
    owner_node   TEXT,              -- node that claimed it
    submitted_at BIGINT NOT NULL,
    started_at   BIGINT,
    finished_at  BIGINT,
    error        TEXT
);
```

Not replicated — claim races are resolved by the dedup layer keyed on
`cache_key`.  The eventual response lands in two places:

1. **Inference cache** (replicated; phase 3).
2. **`ResultQueue`** under `result_id` via `v2/results.push` —
   the SAME per-node queue Bund script evaluations use.  Callers poll
   `v2/results.pull` exactly as they do for `v2/eval.queued` jobs.

**Runner** — one tokio task per node, spawned by bdsnode/main.rs
alongside the gossip / scheduler / anti-entropy loops:

```
loop {
  while semaphore.try_acquire().ok() {
    let job = jobs.claim_one(node_id)?  // None → break
    spawn(run_one(job))                  // permit released on completion
  }
  sleep(poll_interval).await
}
```

`run_one` checks cancellation **twice**: before the provider call
and after (if the operator cancelled while the provider was running,
we record `cancelled` and skip results.push).  We do NOT abort an
in-flight provider HTTP request — the upstream's cost was already
incurred.

Result-queue payload:

```json
{
  "job_id":    "01997d...",
  "result_id": "01997e...",
  "kind":      "complete" | "analyze:rca" | ...,
  "state":     "done" | "failed" | "cancelled",
  "result":    { ...vm::api::llm response Map... },     // when done
  "error":     "..."                                    // when failed
}
```

---

## 7 · `bdscmd llm` subcommand

Every subcommand HMAC-signs under `--secret` (or `BDSCMD_CLUSTER_SECRET`)
— v4/* refuses unsigned requests by design.  Full reference in
[`BDSCMD.md`](BDSCMD.md) § _LLM_.  Quick tour:

```bash
# Sync
bdscmd llm complete  -p "summarise the data we just ingested"
bdscmd llm chat      -m "follow up" --chat-id <uuid> --duration 1h
bdscmd llm analyze   -k rca --duration 1h -q "what broke?"
bdscmd llm analyze   -k documents --id <uuid>,<uuid>
bdscmd llm analyze   -k supplied --rows-file rows.json -q "summarise"
bdscmd llm embed     -t "embed me"
bdscmd llm providers

# Async
bdscmd llm async  -k complete -p "long job"          # returns {job_id, result_id}
bdscmd llm async  -k analyze --analyze-kind rca --duration 1h
bdscmd llm status -i <job-uuid>
bdscmd llm cancel -i <job-uuid>
bdscmd llm jobs   --state pending --limit 50

# Cache admin
bdscmd llm cache stats
bdscmd llm cache purge --provider ollama --older-than-secs 86400
bdscmd llm cache purge                                # empty filter purges everything
```

**Input ergonomics**

| Flag pair                              | When to use                                          |
|----------------------------------------|------------------------------------------------------|
| `--prompt` / `--messages-file`         | `complete`; single-user-message string or JSON `[{role,content},…]` |
| `--message` / `--message-file`         | `chat`; long bodies typically live in a file         |
| `--context` / `--context-file`         | `chat`; pre-built RAG string overrides the inline aggregation |
| `--texts-file`                         | `embed`; one text per line                           |
| `--rows-file`                          | `analyze --kind supplied`; JSON array                |
| `--id` (repeatable or comma-sep)       | `analyze --kind documents`                           |

Provider control (`--provider`, `--model`), generation options
(`--temperature`, `--max-tokens`, `--top-p`, `--seed`), and `--no-cache`
work across every subcommand that accepts them.

---

## 8 · Bund `cls.llm.*` words

All words exist in both **stack** form (`cls.llm.complete`) and
**workbench** form (`cls.llm.complete.`).  See
[`Bund/BDS.md`](Bund/BDS.md) for the broader Bund DB family
convention.

| Word                       | Stack (deepest first) | Result                                |
|----------------------------|-----------------------|---------------------------------------|
| `cls.llm.complete`         | `req(MAP)`            | response Map                          |
| `cls.llm.chat`             | `req(MAP)`            | chat Map (`{chat_id, response, …}`)   |
| `cls.llm.analyze`          | `req(MAP)`            | analyze Map                           |
| `cls.llm.embed`            | `req(MAP)`            | embedding Map (`{vectors, dim, …}`)   |
| `cls.llm.providers`        | –                     | `{default, providers: [...]}`         |
| `cls.llm.complete.async`   | `req(MAP)`            | `{job_id, result_id, kind, state}`    |
| `cls.llm.analyze.async`    | `req(MAP)`            | `{job_id, result_id, kind, state}`    |
| `cls.llm.jobs.list`        | `filter(MAP)`         | `{jobs: [...], count}`                |
| `cls.llm.status`           | `job_id(STR | MAP)`   | job summary                           |
| `cls.llm.cancel`           | `job_id(STR | MAP)`   | `{ok, job_id}`                        |
| `?llm.meta`                | –                     | per-thread LLM meta (or `nodata`)     |

`req` shapes mirror the JSON-RPC bodies — see § 10.

Example Bund script that uses `analyze` to drive an RCA over the
last hour and signals the result:

```bund
[ "kind"     "rca"
  "duration" "1h"
  "query"    "what's failing?"
] $tomap
cls.llm.analyze ! $r

$r "response" $get
"rca-summary" "info" $now $r $signal.emit
```

---

## 9 · bdsweb integration

**`/chat`** (replaces the legacy `v2/chat.ollama` UI):

- Provider dropdown populated from `v4/llm.providers.list` on page load.
- Provider preference sticky via `bds-chat-provider` cookie (HttpOnly,
  SameSite=Strict).
- Context header shows provider + model + cache state + `prompt_chars`
  + `num_ctx` after every turn — surfaces RAG hit count immediately.
- Auto-recovery: a stale `bds-chat-session` cookie pointing at a
  wiped docstore session silently re-opens a new session instead of
  surfacing "session not found" to the user.
- `num_ctx` auto-bumps to 8k/16k/32k/64k based on assembled prompt size,
  preventing Ollama's 2048-token default from silently truncating RAG
  context.

**`/admin/llm`**:

- **Providers** card — table from `v4/llm.providers.list` (id /
  default_model / chat / embed / "★" on the default).
- **Inference cache** card — `v4/llm.cache.stats`: rows / total hits /
  human-formatted response bytes / TTL flag.  Inline purge form (filters:
  provider, kind, older-than-secs; empty = purge everything, with a JS
  confirm guard).
- **Recent async jobs** — `v4/llm.jobs.list?limit=20` with state-coloured
  rows (done=emerald, failed=red, cancelled=amber, running=sky,
  pending=slate).

Routes:

| Route                  | Method | Notes                                          |
|------------------------|--------|------------------------------------------------|
| `/chat`                | GET    | provider picker + chat-session UI              |
| `/chat/new`            | POST   | open new session with RAG-prepped briefing     |
| `/chat/query`          | POST   | follow-up turn                                 |
| `/chat/reset`          | GET    | clear `bds-chat-session` cookie (keep provider)|
| `/admin/llm`           | GET    | banners via `?notice=purged` / `?error=…`      |
| `/admin/llm/purge`     | POST   | HMAC-signed `v4/llm.cache.purge` then redirect |

Full user-facing manual: [`BDS_UI.md`](BDS_UI.md) § _LLM_.

---

## 10 · JSON-RPC surface

### Coordinator (HMAC-signed)

| Method                       | Purpose                                                        |
|------------------------------|----------------------------------------------------------------|
| [`v4/llm.complete`](jsonrpc_api/v4_llm_complete.md)              | Single-shot completion                                         |
| [`v4/llm.chat`](jsonrpc_api/v4_llm_chat.md)                      | Stateful chat turn (history in docstore); optional inline RAG  |
| [`v4/llm.analyze`](jsonrpc_api/v4_llm_analyze.md)                | RAG over `ContextSource` kind + completion                     |
| [`v4/llm.embed`](jsonrpc_api/v4_llm_embed.md)                    | Vector embeddings                                              |
| [`v4/llm.providers.list`](jsonrpc_api/v4_llm_providers_list.md)  | Registered providers + capabilities + default                  |
| [`v4/llm.complete_async`](jsonrpc_api/v4_llm_complete_async.md)  | Enqueue completion for the runner                              |
| [`v4/llm.analyze_async`](jsonrpc_api/v4_llm_analyze_async.md)    | Enqueue analyze for the runner                                 |
| [`v4/llm.jobs.list`](jsonrpc_api/v4_llm_jobs_list.md)            | List queued / in-flight / terminal jobs                        |
| [`v4/llm.jobs.status`](jsonrpc_api/v4_llm_jobs_status.md)        | Inspect one job                                                |
| [`v4/llm.jobs.cancel`](jsonrpc_api/v4_llm_jobs_cancel.md)        | Cancel a pending or running job                                |
| [`v4/llm.cache.stats`](jsonrpc_api/v4_llm_cache_stats.md)        | Cache totals                                                   |
| [`v4/llm.cache.purge`](jsonrpc_api/v4_llm_cache_purge.md)        | Drop rows by filter                                            |

### Internal receivers (HMAC-protected via shared_secret cluster, unauthenticated like other `v2/*` receivers)

| Method                          | Caller                                                        |
|---------------------------------|---------------------------------------------------------------|
| `v2/llm.cache.get`              | `dispatch::read` fan-out for cluster-wide cache lookup (phase 3.d) |
| `v2/llm.cache.get.by_id`        | Anti-entropy `pull_one` for the `llm_cache` store             |
| `v2/llm.cache.put`              | `replicate_to_all` from `vm::api::llm::cache_store`           |
| `v2/llm.cache.list_ids`         | Anti-entropy sweep — same shape as `v2/doc.list_ids`          |
| `v2/llm.cache.delete`           | Tombstone apply path (future use)                             |
| `v2/llm.last_executed`          | Dedup fan-out — peer of `v2/scheduler.last_seen`              |

### Response shape conventions

Every coordinator method returns these fields when applicable:

- `response` (string) — model output (for completion-shaped calls)
- `provider` + `model` — what actually answered
- `cache` (string) — `"hit"` | `"miss"` | `"disabled:*"`
- `dedup` (string, on complete/analyze) — `"ran"` | `"waited"` | `"skipped:done"` | `"disabled"`
- `prompt_chars` (number, on chat) — assembled prompt length
- `num_ctx` (number, on chat) — Ollama context window used
- `tokens_in` / `tokens_out` — provider-reported when available
- `ms` — wall-clock cost (0 on cache hit)

---

## 11 · Diagnostics

### `?llm.meta` Bund word

Returns a Map matching the response's introspection block:

```json
{
  "provider":     "ollama",
  "model":        "llama3.2",
  "ms":           842,
  "tokens_in":    312,
  "tokens_out":   58,
  "prompt_chars": 14823,
  "num_ctx":      32768,
  "kind":         "analyze:rca",   // analyze only
  "n_rows":       12,              // analyze only
  "cache":        "miss",
  "dedup":        "ran"
}
```

Returns `nodata` when no LLM helper has run on this thread yet.

### Bdsnode log lines (level INFO unless marked otherwise)

```
[llm] 1 provider(s) registered: ["ollama"] (default=Some("ollama"))
[llm] cache opened at /var/lib/bds/llm (rows=812, ttl=86400s)
[llm] dedup: enabled=true window=300s wait_max=30s
[llm] job queue opened at /var/lib/bds/llm (pending=0)
[llm_jobs] runner started (node=019e0… max_concurrency=2 poll=1s)

[llm::chat] RAG loaded telemetry=208 docs=11 chars=14823 for duration="1h" query="…"
[llm::chat] sent prompt to provider=ollama model=llama3.2 prompt_chars=14823 num_ctx=32768 (telemetry=208 docs=11 tokens_in=… tokens_out=…)
[llm::chat] RAG returned NO rows for duration="1h" query="…" (telemetry=0 docs=0) — the model will answer without context.   [WARN]
[llm::dedup] release_done failed: …                                                                                          [WARN]
[llm_jobs] job <uuid> cancelled mid-flight — result discarded
```

---

## 12 · Operational gotchas

### Ollama's default `num_ctx` is 2048 tokens

Real-world RAG easily blows past that.  When input exceeds the
configured context window, **Ollama silently truncates from the
start of the prompt**.  Your system message and the leading RAG
context get dropped; only the trailing "User question: …" survives.
The model answers from general knowledge and the operator can't
tell.

`vm::api::llm::chat` / `analyze` auto-pick a `num_ctx` bucket based
on assembled prompt size (8k / 16k / 32k / 64k).  Override per call
with `options.num_ctx: <N>` if you need something specific.  Watch
the `num_ctx` field in the response or `?llm.meta` to confirm what
was sent.  bdsweb chat header surfaces it after every turn.

### Cache replication needs `full_replication_stores` to include `"llm_cache"`

The library default is fine.  Setups that override
`cluster.full_replication_stores` in `bds.hjson` must include both
`"users"` and `"llm_cache"` for those stores to anti-entropy across
the cluster.

### `temperature > 0` disables caching

By design — non-deterministic outputs shouldn't be cached.  If you
want a hit anyway, set `temperature: 0` (or omit the field) and rerun.

### `cluster.shared_secret` is mandatory for v4/*

Every v4/llm.* method requires HMAC.  Open-access bdsweb mode (no
shared_secret) means v4 calls fail at the auth gate.  The chat page
shows a banner explaining the situation.  bdscmd llm bails with a
clear error message instead of attempting an unsigned call.

### Chat turns aren't cacheable

Every chat turn extends the running history, so the canonical
request never repeats.  The `cache: "disabled:chat"` label makes the
absence explicit rather than silently treating it as a miss.

### Stale chat cookies after `bdsnode --new`

A `bds-chat-session` cookie can outlive the docstore.  `v4/llm.chat`
silently re-opens a new session in that case (the response carries
the fresh `chat_id` and the cookie auto-updates on the next turn)
rather than surfacing "session not found".
