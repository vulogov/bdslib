# v2/status

Return a live snapshot of the bdsnode process: identity, uptime, wall-clock time, and the current depth of the two ingest queues.

The response is assembled synchronously from in-process state — no database or shard access is performed. The call always succeeds as long as the server is running.

## Parameters

This method accepts no parameters.  The `params` field may be omitted or set to an empty object.

## Response

```json
{
  "node_id":         "0196f3a2-1b4c-7e2d-9f0a-3c5b6d8e1f2a",
  "hostname":        "bds-prod-01.example.com",
  "uptime_secs":     3724,
  "timestamp":       1745003724,
  "logs_queue":      14,
  "json_file_queue": 2,
  "json_file_name":  "/var/log/ingest/2026-04-25T12:00:00.ndjson"
}
```

| Field | Type | Description |
|---|---|---|
| `node_id` | string | Stable node identifier. Set at startup from the `--nodeid` CLI flag, or auto-generated as a UUID v7 when the flag is omitted. |
| `hostname` | string | Hostname of the machine running bdsnode. Resolved at startup from `$HOSTNAME`, then `/etc/hostname`, then the `hostname` command. `"unknown"` if none of those sources are available. |
| `uptime_secs` | integer | Seconds elapsed since bdsnode started. |
| `timestamp` | integer | Current wall-clock time as Unix seconds (UTC). |
| `logs_queue` | integer | Number of JSON telemetry documents currently queued in the `"ingest"` pipe, waiting to be flushed to the shard store by the `bds-add` background thread. |
| `json_file_queue` | integer | Number of file paths currently queued in the `"ingest_file"` pipe, waiting to be processed by the `bds-add-file` background thread. |
| `json_file_name` | string \| null | Absolute path of the file currently being ingested by the `bds-add-file` thread. `null` when no file is being processed (idle, or file ingest is disabled in config). |

## Example

```bash
curl -s -X POST http://127.0.0.1:9000 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/status","params":{},"id":1}' | jq
```

```json
{
  "jsonrpc": "2.0",
  "result": {
    "node_id":         "0196f3a2-1b4c-7e2d-9f0a-3c5b6d8e1f2a",
    "hostname":        "bds-prod-01.example.com",
    "uptime_secs":     3724,
    "timestamp":       1745003724,
    "logs_queue":      14,
    "json_file_queue": 2,
    "json_file_name":  "/var/log/ingest/2026-04-25T12:00:00.ndjson"
  },
  "id": 1
}
```

### Idle node (no ingest activity)

```json
{
  "jsonrpc": "2.0",
  "result": {
    "node_id":         "0196f3a2-1b4c-7e2d-9f0a-3c5b6d8e1f2a",
    "hostname":        "bds-prod-01.example.com",
    "uptime_secs":     86400,
    "timestamp":       1745086400,
    "logs_queue":      0,
    "json_file_queue": 0,
    "json_file_name":  null
  },
  "id": 1
}
```

## Error responses

This method does not produce application-level errors.  The only failure mode is an internal server panic (`-32000`), which indicates the server is in an unrecoverable state.

## Notes

- **Node identity.** The `node_id` is fixed for the lifetime of the process.  Use `--nodeid <value>` to assign a stable, human-readable name (e.g. `bds-primary`, `region-eu-west-1`) for use in dashboards or alerting rules.  When the flag is omitted, a fresh UUID v7 is generated each time bdsnode starts.
- **Queue depths.** `logs_queue` and `json_file_queue` reflect messages that have been accepted by the RPC layer but not yet written to storage.  A non-zero queue is normal under load.  A persistently growing queue indicates the ingest thread cannot keep up with the ingestion rate.
- **File ingest disabled.** When the `file_batch_size` / `file_timeout_ms` config keys are absent, the `bds-add-file` thread is not started and `json_file_queue` will always be `0` while `json_file_name` will always be `null`.
- **Polling.** `v2/status` is lightweight and safe to poll at high frequency (e.g. every second) for monitoring dashboards.  It never touches the database.
- **Timestamp vs uptime.** `timestamp` is an absolute Unix epoch value useful for correlating with external systems.  `uptime_secs` is useful for tracking restarts — if `uptime_secs` resets unexpectedly while `node_id` stays the same (fixed `--nodeid`), the process was restarted.
