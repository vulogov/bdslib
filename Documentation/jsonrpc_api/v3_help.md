# v3/help

Docstore-backed Q&A over the cluster.  Takes an English `message`,
retrieves the top-`limit` matching documents from the docstore,
optionally filters to internal-only documents, then asks the default
LLM provider to answer using those documents as RAG context.

Because the docstore is one of the **fully-replicated** cluster
stores (alongside `signals` / `scripts` / `users` / `llm_cache`), a
local doc search already covers every peer's content — there's no
fan-out in this endpoint and no `cluster_meta` in the response.

Unauthenticated v3/* read surface — matches `v3/search` /
`v3/aggregationsearch` conventions.  The trust boundary is the
bdsnode RPC port.  See [`../LLM.md`](../LLM.md) § 14 for the
architectural overview and [`../SCRIPTS.md`](../SCRIPTS.md) §
`load_internal_documentation.sh` for the operator-curated corpus
this endpoint is designed to query.

## Parameters

| Field           | Type    | Required | Description |
|-----------------|---------|----------|-------------|
| `message`       | string  | yes      | The English question.  Trimmed; empty/whitespace-only returns error `-32004`. |
| `internal_only` | bool    | no       | When `true`, restrict RAG to documents whose `metadata.internal_doc == true` (the tag emitted by `scripts/load_internal_documentation.sh`).  Default: `false`. |
| `limit`         | int     | no       | Number of documents to include in the prompt.  Server-clamped to `[1, 50]`.  Default: `8`. |
| `provider`      | string  | no       | Override the cluster's default provider (`""` / omitted → `llm.default`). |
| `model`         | string  | no       | Override the provider's default model (`""` / omitted → provider's `default_model`). |
| `options`       | object  | no       | Provider sampling block (`temperature`, `max_tokens`, `top_p`, `seed`, `num_ctx`, `stop[]`).  `num_ctx` is auto-bucketed by prompt size when omitted (16k / 32k / 64k). |

When `internal_only=true` the server over-fetches (4 × `limit`) to
give the post-filter step enough material to fill the requested
slot count even on corpora where internal docs are rare.

## Response

| Field           | Type    | Notes |
|-----------------|---------|-------|
| `answer`        | string  | LLM answer body (trailing whitespace trimmed).  The system prompt instructs the model to cite document names in `[brackets]` and refuse when the corpus doesn't answer the question. |
| `n_docs`        | int     | Number of documents that ended up in the prompt after `internal_only` filtering + truncation.  `0` when nothing matched. |
| `internal_only` | bool    | Echo of the request flag. |
| `limit`         | int     | The **clamped** limit actually used. |
| `sources`       | array   | One entry per cited document, in score order (descending).  Each entry carries `{id, name, score, internal_doc}` — see below. |
| `provider`      | string  | The provider id that answered (`"ollama"` / `"anthropic"` / …). |
| `model`         | string  | The model id reported by the provider. |
| `ms`            | int     | Wall-clock cost in milliseconds. |
| `tokens_in`     | int     | Provider-reported when available. |
| `tokens_out`    | int     | Provider-reported when available. |
| `note`          | string  | **Only present when `n_docs == 0`.**  Short human-readable explanation (`"no internal documents matched — answer based on the model's general knowledge"` etc.). |

### `sources[]` entry

| Field          | Type   | Notes |
|----------------|--------|-------|
| `id`           | string | Document UUIDv7. |
| `name`         | string | `metadata.name` if present, else `metadata.path`, else `"<no name>"`. |
| `score`        | float  | Cosine similarity score from the HNSW vector search. |
| `internal_doc` | bool   | `true` when `metadata.internal_doc == true`. |

## Example

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{
    "jsonrpc":"2.0","id":1,
    "method":"v3/help",
    "params":{
      "message":"What does the v2/to.bund endpoint do?",
      "internal_only":true,
      "limit":4
    }
  }' | jq
```

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "answer": "According to [document 2 — v2_to_bund.md], the v2/to.bund endpoint hands a natural-language request to the configured LLM provider, validates the returned ```bund``` block through bund_parse + an undefined-word dry-run, and retries on failure up to llm.to_bund.max_retries times…",
    "n_docs": 4,
    "internal_only": true,
    "limit": 4,
    "sources": [
      { "id": "019e2200-…", "name": "v2_to_bund.md",  "score": 0.51, "internal_doc": true },
      { "id": "019e2201-…", "name": "LLM.md",          "score": 0.47, "internal_doc": true },
      { "id": "019e2202-…", "name": "SYNTAX_AND_VM.md","score": 0.43, "internal_doc": true },
      { "id": "019e2203-…", "name": "v3_script_update.md", "score": 0.41, "internal_doc": true }
    ],
    "provider": "ollama",
    "model":    "llama3.2",
    "ms":       6993,
    "tokens_in":  4698,
    "tokens_out": 209
  }
}
```

## Prompt assembly

The system prompt is baked into the library
(`src/llm/help.rs::SYSTEM_PROMPT`) and instructs the model to:

- Use **only** the provided documents.
- Cite document names in `[brackets]`.
- Say so plainly when the corpus doesn't answer the question
  (instead of guessing from general knowledge).
- Stay concise — a few sentences when possible, bullets / short code
  blocks for procedural questions.
- Quote command lines / paths / config keys **verbatim** from the
  documents.

Each document is truncated to `MAX_CONTENT_CHARS` (8 000 chars) when
spliced into the user turn so a single 150-KB markdown file can't
blow the prompt budget.  Truncation respects UTF-8 codepoint
boundaries and appends a short `[…content truncated]` marker.

## v3/help.settings

Companion read-only RPC.  Echoes the defaults so operators can
sanity-check the limit ceiling and the underlying provider manager
without making a real Q&A call.

### Parameters

None.

### Response

```json
{
  "default_limit":    8,
  "max_limit":        50,
  "default_provider": "ollama",
  "providers":        ["ollama", "anthropic"]
}
```

`default_provider` is empty when no provider is registered.

## Error codes

| Code     | Trigger                                                                              |
|----------|--------------------------------------------------------------------------------------|
| `-32000` | task panic on the bdsnode tokio pool                                                 |
| `-32004` | empty `message` · provider manager not initialised · provider call failed · docstore search failed |

The endpoint deliberately does **not** error on `n_docs == 0` —
the model is still asked, with a corrective system-prompt clause,
and the response carries a `note` so callers can warn the user.

## See also

- [`../LLM.md`](../LLM.md) — full LLM surface reference (§ 14 covers `v3/help`)
- [`../SCRIPTS.md`](../SCRIPTS.md) § `load_internal_documentation.sh` —
  load the operator-curated Documentation/ corpus that this endpoint
  is built to query
- [`v4_llm_analyze.md`](v4_llm_analyze.md) — the RAG cousin that
  takes pre-resolved doc ids (no search step) and requires HMAC
