# v4/llm.providers.list

Registered providers + the default.  HMAC-protected (every v4/llm.* is).

Reads the process-wide `ProviderManager` populated at bdsnode startup
from the `llm.providers` block in `bds.hjson`.  Providers whose
`api_key_env` was unset at boot are logged-and-skipped and won't
appear here.

## Parameters

| Field   | Type   | Required | Description |
|---------|--------|----------|-------------|
| `_hmac` | string | yes      | HMAC over (empty) canonical params |

## Response

```json
{
  "default": "ollama",
  "providers": [
    {
      "id":            "ollama",
      "default_model": "llama3.2",
      "capabilities": { "chat": true,  "embed": true  }
    },
    {
      "id":            "anthropic",
      "default_model": "claude-sonnet-4-5",
      "capabilities": { "chat": true,  "embed": false }
    },
    {
      "id":            "openai",
      "default_model": "gpt-4o-mini",
      "capabilities": { "chat": true,  "embed": true  }
    }
  ]
}
```

`default` is `null` when nothing is registered (e.g. config has no
`llm.providers` block).

## Example

```bash
bdscmd llm providers
```

bdsweb's `/chat` page calls this on every page load to populate the
provider dropdown; `/admin/llm` uses it for the Providers card.
