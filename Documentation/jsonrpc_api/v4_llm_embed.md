# v4/llm.embed

Vector embeddings for one or more texts.  HMAC-protected.

Capability requirement: the resolved provider's
`capabilities().embed` must be `true`.  Ollama and OpenAI implement
embeddings; Anthropic does not and returns an error.

## Parameters

| Field      | Type   | Required | Description |
|------------|--------|----------|-------------|
| `text`     | string | one of   | Single text — forwarded as `texts: [text]` |
| `texts`    | list   | one of   | List of strings |
| `provider` | string | no       | Provider override |
| `model`    | string | no       | Provider-specific embedding model id |
| `_hmac`    | string | yes      | |

## Response

```json
{
  "vectors": [[0.012, -0.083, …], [-0.001, 0.247, …]],
  "dim":     768,
  "provider":"ollama",
  "model":   "nomic-embed-text",
  "ms":      214
}
```

## Errors

| Code | Condition |
|---|---|
| `-32004` | Provider error / embed not supported / empty `texts` |
| `-32602` | Neither `text` nor `texts` supplied |

## Example

```bash
bdscmd llm embed -t "embed this string" --provider ollama
bdscmd llm embed --texts-file lines.txt
```
