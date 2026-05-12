# v4/llm.chat

Stateful chat turn.  Chat history is persisted in the docstore as a
JSON array of `{role, content}` messages under a document whose
metadata records the session's provider + model + system prompt.
HMAC-protected.

Replaces the legacy [`v2/chat.ollama`](v2_chat_ollama.md) — same
response shape so the bdsweb chat UI swapped over without field
renames.  See [`../LLM.md`](../LLM.md) § _Provider layer_ and
§ _bdsweb integration_.

## Parameters

| Field           | Type   | Required | Description |
|-----------------|--------|----------|-------------|
| `message`       | string | yes      | The operator's turn. |
| `chat_id`       | string | no       | UUID of an existing session.  Omit / `null` to open a new one.  **Auto-recovery:** if the id doesn't match a session in the docstore, a fresh one is silently opened (the response carries the new id). |
| `provider`      | string | no       | Per-turn override; falls back to session metadata, then to `llm.default`. |
| `model`         | string | no       | Per-turn override. |
| `system_prompt` | string | no       | Seeded into a NEW session only — ignored for follow-up turns. |
| `duration`      | string | no       | Humantime window (e.g. `"1h"`).  When set without `context`, runs the cluster-aware `aggregation_search` and prepends the top-N fingerprints to `message`. |
| `context`       | string | no       | Verbatim RAG context that REPLACES the inline aggregation pass. |
| `options`       | object | no       | `{temperature, max_tokens, top_p, stop[], seed, num_ctx}` |
| `_hmac`         | string | yes      | HMAC-SHA256 over canonical params. |

## Response

```json
{
  "chat_id":         "01997e92-3fd8-7…",
  "response":        "…model output…",
  "provider":        "ollama",
  "model":           "llama3.2",
  "is_new_session":  false,
  "telemetry_count": 208,
  "document_count":  11,
  "prompt_chars":    14823,
  "num_ctx":         32768,
  "finish_reason":   "stop",
  "tokens_in":       4112,
  "tokens_out":      241,
  "ms":              2104,
  "cache":           "disabled:chat"
}
```

`cache: "disabled:chat"` is intentional — chat turns extend running
history and the canonical request never repeats, so caching makes no
sense.

`prompt_chars` + `num_ctx` are the diagnostics added to catch the
common Ollama "default `num_ctx=2048` silently truncates RAG" trap
— see [`../LLM.md`](../LLM.md) § _Operational gotchas_.

## Errors

| Code | Condition |
|---|---|
| `-32001` | `ShardsManager` not initialised |
| `-32097` | Cluster mode disabled |
| `-32098` | Missing / invalid HMAC |
| `-32004` | Provider error |
| `-32602` | Missing required `message` |

## RAG sourcing

Priority (highest first):

1. `context` (verbatim) — no DB hit
2. `duration` (with `query` falling back to `message`) — runs
   `vm::api::search::aggregation_search` (cluster-aware) and joins
   the fingerprints into a `[telemetry N]` / `[document N]` list
   prepended to the user message
3. Neither — bare user message

When both telemetry and documents return zero rows, a WARN line is
logged:

```
[llm::chat] RAG returned NO rows for duration="1h" query="…"
            (telemetry=0 docs=0) — the model will answer without
            context.  Check that `cluster.full_replication_stores`
            and the search index actually cover the queried window.
```

bdsweb chat surfaces the same condition in the header banner:

```
⚠ NO RAG context loaded for last 1h — model is answering without
your data · provider=ollama model=llama3.2
```

## Example

```bash
# Open a new session, RAG-prepped over the last hour
curl -s -X POST http://localhost:9000 -H 'Content-Type: application/json' -d '{
  "jsonrpc":"2.0","id":1,"method":"v4/llm.chat","params":{
    "message":  "what should I investigate?",
    "duration": "1h",
    "_hmac":    "<sha256 hex>"
  }
}'
```
