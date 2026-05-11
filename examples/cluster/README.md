# Cluster-aware Bund VM examples

Eight short scripts demonstrating the `cls.*` family of cluster-aware
Bund stdlib words, plus the `?cluster.meta` introspection word.

| Script                       | What it shows                                           |
|------------------------------|---------------------------------------------------------|
| `01_meta.bund`               | The `?cluster.meta` cache after a no-arg cluster read   |
| `02_add_count.bund`          | `cls.add` (sharded write) + `cls.count` (sum read)      |
| `03_search.bund`             | `cls.fulltext`, `cls.search`, `cls.aggregation`         |
| `04_analysis.bund`           | `cls.timeline`, `cls.trends`, `cls.knn`                 |
| `05_signals.bund`            | `cls.signal.emit` (replicated write) + `cls.signals.recent` |
| `06_documents.bund`          | `cls.doc.add` (replicated write) + `cls.doc.get.metadata`|
| `07_keys_primaries.bund`     | `cls.keys`, `cls.primaries`, `cls.primaries.explore`    |
| `08_introspection.bund`      | Tour through `?cluster.meta` states                     |

## Running them

The scripts are sent to a running `bdsnode` via the `v2/eval` JSON-RPC
method.  In standalone mode each `cls.*` word runs the local DB call
and `?cluster.meta` returns nodata.  In cluster mode each `cls.*` read
fans out to every Alive peer and merges; writes replicate to peers
with hinted-handoff.  The script doesn't change.

### Stand up a 3-node test cluster

```bash
make all                                                    # cargo build
rm -rf /tmp/bds-cluster && mkdir -p /tmp/bds-cluster

BDS_CONFIG=bds1.hjson ./target/debug/bdsnode --new --port 9711 -d 1 &
BDS_CONFIG=bds2.hjson ./target/debug/bdsnode --new --port 9712 -d 2 &
BDS_CONFIG=bds3.hjson ./target/debug/bdsnode --new --port 9713 -d 3 &
```

Wait a few seconds for the gossip layer to seat the peer table.

### Send an example script

```bash
SCRIPT=$(cat examples/cluster/01_meta.bund)
curl -s -X POST http://127.0.0.1:9711 \
  -H "Content-Type: application/json" \
  -d "$(jq -nc --arg s \"\$SCRIPT\" '{
        jsonrpc: \"2.0\",
        id: 1,
        method: \"v2/eval\",
        params: { context: \"demo\", script: \$s }
      }')" | jq .
```

The `result.result` field of the response is whatever was on the
top of the workbench when the script finished.  The example scripts
all push `?cluster.meta.` (workbench-targeted) as the final word so
the response carries the cluster meta.

### Reading the response

A cluster READ that succeeded against a 3-node cluster:

```json
{
  "enabled": true,
  "peers_queried": 2,
  "peers_answered": 2,
  "partial": false,
  "failed": []
}
```

A cluster WRITE that hit every peer:

```json
{
  "enabled": true,
  "replication": {
    "peers_attempted": 2,
    "peers_succeeded": 2,
    "hints_queued": 0
  }
}
```

A local-only helper, or standalone mode:

```json
null
```

## How it works

Every `cls.*` word is a thin wrapper around a `vm::api::*` helper
defined in `src/vm/api/`.  The helper:

1. Looks up the global `ShardsManager` via `bdslib::get_db()`.
2. Runs the local DB call.
3. If `db.cluster()` is `None` → returns the local result, clears the
   per-thread meta.
4. Otherwise drives `cluster::fanout::fan_out_v2` to completion (using
   `tokio::task::block_in_place` from inside `spawn_blocking`), merges
   via the per-method strategy in `cluster::merge`, stashes the
   resulting `cluster_meta` block on a thread-local cell, and returns
   the merged JSON converted back to a `rust_dynamic::value::Value`.

The Bund script never sees the JSON conversion or the cluster code
path — it just gets a Map / List / String it can `get`, `len`, `iter`
over.  `?cluster.meta` makes the choice between local and cluster
visible only when the script asks for it.

## Building Map arguments in Bund

Several `cls.*` words take a Map (`opts`, the `extra` block of
`cls.signal.emit`, the doc metadata for `cls.doc.add`, …).  Bund's
`set` word builds Maps by accumulation:

```bund
dict                     // push empty map
"key1" value1   set      // pulls value1 (top), "key1" (mid), dict (bot)
"key2" value2   set      // each set returns the updated map
"key3" value3   set
```

After three `set` calls the stack has a Map with three entries.
**Note** the push order: `key_string`, then `value`, then `set`.  The
key MUST be a string; values can be any Bund type.
