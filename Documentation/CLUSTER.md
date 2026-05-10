# Cluster mode (Phases 1–6)

`bdsnode` can run as a peer in a P2P, serverless cluster.  The cluster
layer is now feature-complete across six phases:

| Phase | What shipped |
|---|---|
| 1 | Membership + discovery (gossip, peer table, HMAC auth) |
| 2 | Distributed reads + analytics fan-out (`v3/timeline`, `v3/count`, `v3/search`, `v3/knn`, `v3/anomaly.recent`, `v3/denoise.recent`) |
| 3 | Sharded write replication (`v3/add`, `v3/add.batch`) with hinted handoff |
| 4 | Fully-replicated stores (`v3/doc.*`, `v3/signal.emit`, `v3/script.*`) with tombstones + anti-entropy |
| 5 | Operations polish: `v3/cluster.sync`, AE telemetry, distinct-count, LWW for v3 updates, signal anti-entropy fill, dashboard improvements |
| 6 | Cluster-wide read coverage for the rest of the v2/* read surface: `v3/add.file*`, `v3/fulltext*`, `v3/keys*`, `v3/primaries*`, `v3/topics*`, `v3/signals*`, `v3/tpl.*` (read), `v3/search.get` |

This document covers the design, configuration, and operational tooling
of the cluster layer in its current form.

## 1. Why a cluster?

Cluster mode is **opt-in**. With `cluster.enabled = false` (the default), every
existing single-node deployment continues to work unchanged — no background
tasks run, no extra files are created on disk.

When enabled, every bdsnode in the mesh:

- Discovers other peers from a single bootstrap address (or a previously
  saved peer list).
- Periodically gossips with a random alive peer to confirm liveness and
  converge the membership table.
- Surfaces cluster identity and topology through `v2/status`,
  `v2/cluster.peers`, the v3/cluster.* RPCs, the bdsweb `/cluster` page, and
  `bdscmd cluster`.

This is the foundation Phases 2+ build replication and distributed query on
top of.

## 2. Modes

`Cluster::mode()` returns one of:

| Mode | Trigger | Phase 1 behaviour |
|---|---|---|
| `standalone` | 0 alive peers | Local-only operations; gossip ticks are quiet no-ops |
| `partial`    | 1..=`full_mode_threshold-1` alive peers | Gossip runs; later phases will use best-effort replication here |
| `full`       | ≥ `full_mode_threshold` alive peers | Gossip runs; later phases will enforce the configured replication factor |

Phase 1 prints the mode in `v2/status.cluster.mode`, on the bdsweb `/cluster`
page, and via `bdscmd cluster status`. Replication is a no-op everywhere — the
mode is informational until Phase 3 lands `v3/add` / `v3/add.batch`.

## 3. Discovery

**Bootstrap pass** (one-shot, at startup):

1. The node loads (or generates) its stable `node_id` from
   `<dbpath>/network/node_id`.
2. It collects bootstrap targets:
   - `cluster.bootstrap` from the config (if set), and
   - every entry from the persisted `<dbpath>/network/peers.json`.
3. It calls `v3/cluster.hello` against each target in turn, exchanging
   identity and the remote peer view.

**Gossip loop** (every `gossip_interval`, default 5 s):

1. Sweep the peer table: peers not seen for `suspect_timeout` transition to
   `Suspect`; peers not seen for `dead_timeout` transition to `Dead`.
2. Pick a random `Alive` peer.
3. `v3/cluster.ping` it (1–2 s timeout).
4. Every 3rd tick, also `v3/cluster.peers` and merge the result.
5. **Recovery probe**: pick a random `Suspect`/`Dead` peer and ping it. On
   success, transition it back to `Alive` and persist. Without this step a
   peer marked Dead would never be re-checked, because the regular gossip
   tick only picks Alive peers — so a transient outage (or a fan-out blip)
   could leave the cluster stuck in Standalone mode even after every peer
   has come back. No-op when every peer is already Alive.

The peer table is persisted to disk on every successful merge, so a restart
can reconnect without depending on the configured bootstrap being up.

## 4. Authentication

All `v3/cluster.*` traffic is signed with HMAC-SHA256 using
`cluster.shared_secret`. The signature is carried in the `_hmac` field of the
JSON-RPC params; the receiver re-canonicalises the params (with `_hmac`
removed), recomputes the MAC, and compares in constant time.

| Failure | Code |
|---|---|
| Missing `_hmac` | `-32098` |
| Bad `_hmac` (wrong secret or tampered body) | `-32098` |
| Cluster mode disabled on the receiver | `-32097` |
| Embedding-model mismatch in `cluster.hello` | `-32096` |

Embedding-model mismatch is rejected at the `cluster.hello` layer because
HNSW indexes are dimension-locked at first vector insert — federated vector
search across mixed-dimension peers would silently return wrong results.

The unauthenticated `v2/cluster.peers` endpoint exists for **local trusted
clients** (bdsweb's `/cluster` page, observability dashboards). It exposes
the same view as `v3/cluster.peers` but without the secret requirement —
acceptable because callers must already be able to hit the bdsnode HTTP
endpoint, which itself is the trust boundary.

## 5. Configuration

Add a `cluster` block to `bds.hjson`:

```hjson
cluster: {
  enabled:                true                            // opt-in (default false)
  shared_secret:          "change-me-32-bytes-or-more"    // required when enabled
  bootstrap:              "http://10.0.0.5:9000"          // optional
  bind_url:               "http://10.0.0.7:9000"          // how peers reach us

  gossip_interval:        "5s"
  suspect_timeout:        "30s"
  dead_timeout:           "120s"
  full_mode_threshold:    3

  // ── Phase 3+ knobs (parsed now, not yet enforced) ──
  replication_factor:        3
  full_replication_stores:   ["docs", "signals", "scripts"]
  antientropy_interval:      "5min"
  hint_replay_interval:      "10s"
  hint_max_age:              "24h"
  peer_rpc_timeout:          "2s"
  max_fingerprints_per_peer: 100000
}
```

| Key | Default | Description |
|---|---|---|
| `enabled` | `false` | Master switch. With `false`, the cluster layer is not constructed and no background tasks run. |
| `shared_secret` | required when enabled | HMAC key shared by every peer. Minimum 16 chars. Treat as sensitive — do not commit. |
| `bootstrap` | none | Optional URL of a known peer. Absent means "first node in the cluster". |
| `bind_url` | required when enabled | URL other peers should use to reach us. Goes into `cluster.hello`. |
| `gossip_interval` | `"5s"` | Tick cadence. Each tick = liveness sweep + random ping. |
| `suspect_timeout` | `"30s"` | Time without contact before a peer transitions Alive → Suspect. |
| `dead_timeout` | `"120s"` | Time without contact before a peer transitions to Dead. |
| `full_mode_threshold` | `3` | Alive-peer count at which mode flips to `full`. |
| `replication_factor` | `3` | (Phase 3) Target replica count for `v3/add`. |
| `full_replication_stores` | `["docs","signals","scripts"]` | (Phase 4) Stores that replicate to **every** alive peer. |
| `peer_rpc_timeout` | `"2s"` | Per-peer RPC deadline for gossip and (Phase 2) fan-out reads. |

## 6. On-disk layout

All cluster artefacts live under `<dbpath>/network/`:

```
<dbpath>/
  shards/                  # unchanged
  docs/                    # unchanged
  signals/                 # unchanged
  scripts/                 # unchanged
  network/
    node_id                # this node's stable UUIDv7 (Phase 1)
    peers.json             # last-known peer table; atomic write via tmp+rename (Phase 1)
    hints.duckdb           # hinted-handoff queue used by v3 fan-out (Phase 3+4)
    tombstones.duckdb      # tombstone log for fully-replicated stores (Phase 4)
```

## 7. RPC surface

| Method | Auth | Phase | Purpose |
|---|---|---|---|
| `v2/cluster.peers` | none | 1 | Local-trust read of cluster status + peer table (used by bdsweb). |
| `v2/status` (extended) | none | 1 | Now includes a `cluster` block (or `null`). |
| `v3/cluster.hello` | HMAC | 1 | Handshake. Caller sends identity; receiver echoes its identity + peer view. |
| `v3/cluster.peers` | HMAC | 1 | Return the receiver's full peer table. |
| `v3/cluster.ping` | HMAC | 1 | Lightweight liveness probe. |
| `v3/cluster.status` | HMAC | 1 | Mode, peer counts, replication factor in effect. |
| `v2/fingerprints.recent` | none | 2 | Raw `(uuid, fingerprint)` pairs; input source for v3 distributed analytics. |
| `v3/timeline` | none | 2 | Cluster-wide earliest+latest timestamps (min/max merge). |
| `v3/count` | none | 2 | Cluster-wide record count (sum across peers). |
| `v3/search` | none | 2 | Cluster-wide semantic vector search; UUID dedup + score average. |
| `v3/knn` | none | 2 | Cluster-wide k-NN over the union of every peer's fingerprints. |
| `v3/anomaly.recent` | none | 2 | Cluster-wide n-gram anomaly detection. |
| `v3/denoise.recent` | none | 2 | Cluster-wide n-gram noise removal. |
| `v2/add.batch` (extended) | none | 3 | Now accepts `sync: true` for synchronous batch ingest (used by v3/add.batch fan-out). |
| `v3/add` | none | 3 | Replicated single-document write: local sync + fire-and-forget fan-out + hinted handoff. |
| `v3/add.batch` | none | 3 | Replicated batch write: same recipe as v3/add, one round-trip per peer. |
| `v2/{doc,signal,script}.list_ids` | none | 4 | UUID + `updated_at` enumeration plus tombstones — input source for anti-entropy. |
| `v2/doc.add` (extended) | none | 4 | Now accepts optional `id` for caller-supplied UUID (idempotent receiver returns `existing: true` on retry). |
| `v2/signal.emit` (extended) | none | 4 | Same `id` extension. |
| `v2/script_add` (extended) | none | 4 | Same `id` extension. |
| `v2/doc.delete` (extended) | none | 4 | Now writes a tombstone when cluster mode is on; accepts optional `deleted_at` for shared timestamps across replicas. |
| `v2/script_delete` (extended) | none | 4 | Same tombstone + `deleted_at` extension. |
| `v3/doc.add` | none | 4 | Fully-replicated docstore add — fan-out to **every** Alive peer with shared UUID. |
| `v3/doc.update.metadata` | none | 4 | Fully-replicated metadata update; bumps `metadata.updated_at` for LWW. |
| `v3/doc.update.content` | none | 4 | Fully-replicated content update. |
| `v3/doc.delete` | none | 4 | Fully-replicated delete with shared tombstone (anti-entropy can't resurrect). |
| `v3/signal.emit` | none | 4 | Fully-replicated signal emit (signals are append-only). |
| `v3/script.add` | none | 4 | Fully-replicated BUND script add. |
| `v3/script.update` | none | 4 | Fully-replicated BUND script update. |
| `v3/script.delete` | none | 4 | Fully-replicated BUND script delete with tombstone. |
| `v2/cluster.peers` (extended) | none | 5 | Now exposes per-peer hint counts, tombstone total, AE/hint-tick stats. |
| `v3/cluster.status` (extended) | HMAC | 5 | Adds `hint_backlog`, `tombstone_total`, AE/hint-tick stats. |
| `v3/cluster.sync` | HMAC | 5 | Admin RPC: force an immediate hint replay + AE tick. |
| `v2/signal.get` | none | 5 | Fetch a signal's metadata by UUID — input source for anti-entropy. |
| `v3/count` (extended) | none | 5 | New `distinct: true` mode: UUID-set union for accurate counts under replication. |
| `v2/doc.update.metadata` (extended) | none | 5 | New `if_newer: true` flag: only apply update when incoming `metadata.updated_at` > existing. |
| `v2/script_update` (extended) | none | 5 | Same `if_newer` extension. |

**Why v3 reads are unauthenticated.** v3/* read methods are
*client-to-coordinator* — a client hits the coordinator's v3 endpoint the
same way it hits v2 endpoints, and the coordinator's outbound calls to
peers are plain v2/* (also unauthenticated). The v3/cluster.* methods
require HMAC because they are the *peer-to-peer* gossip protocol and
membership decisions need authenticated identity.

Per-method JSON shapes are documented under `Documentation/jsonrpc_api/`:

- Membership (Phase 1): [`v2_cluster_peers.md`](jsonrpc_api/v2_cluster_peers.md),
  [`v3_cluster_hello.md`](jsonrpc_api/v3_cluster_hello.md),
  [`v3_cluster_peers.md`](jsonrpc_api/v3_cluster_peers.md),
  [`v3_cluster_ping.md`](jsonrpc_api/v3_cluster_ping.md),
  [`v3_cluster_status.md`](jsonrpc_api/v3_cluster_status.md)
- Distributed reads (Phase 2): [`v3_timeline.md`](jsonrpc_api/v3_timeline.md),
  [`v3_count.md`](jsonrpc_api/v3_count.md),
  [`v3_search.md`](jsonrpc_api/v3_search.md)
- Distributed analytics (Phase 2): [`v2_fingerprints_recent.md`](jsonrpc_api/v2_fingerprints_recent.md),
  [`v3_knn.md`](jsonrpc_api/v3_knn.md),
  [`v3_anomaly_recent.md`](jsonrpc_api/v3_anomaly_recent.md),
  [`v3_denoise_recent.md`](jsonrpc_api/v3_denoise_recent.md)
- Replicated writes (Phase 3): [`v3_add.md`](jsonrpc_api/v3_add.md),
  [`v3_add_batch.md`](jsonrpc_api/v3_add_batch.md)
- Fully-replicated stores (Phase 4): [`v2_doc_list_ids.md`](jsonrpc_api/v2_doc_list_ids.md),
  [`v3_doc_add.md`](jsonrpc_api/v3_doc_add.md),
  [`v3_doc_update_metadata.md`](jsonrpc_api/v3_doc_update_metadata.md),
  [`v3_doc_update_content.md`](jsonrpc_api/v3_doc_update_content.md),
  [`v3_doc_delete.md`](jsonrpc_api/v3_doc_delete.md),
  [`v3_signal_emit.md`](jsonrpc_api/v3_signal_emit.md),
  [`v3_script_add.md`](jsonrpc_api/v3_script_add.md),
  [`v3_script_update.md`](jsonrpc_api/v3_script_update.md),
  [`v3_script_delete.md`](jsonrpc_api/v3_script_delete.md)
- Operations (Phase 5): [`v3_cluster_sync.md`](jsonrpc_api/v3_cluster_sync.md),
  [`v2_signal_get.md`](jsonrpc_api/v2_signal_get.md)

## 8. Operations

**bdscmd:**

```bash
# Membership subcommands require the shared secret (HMAC).
export BDSCMD_CLUSTER_SECRET="change-me-32-bytes-or-more"
bdscmd --address http://10.0.0.7:9000 cluster status
bdscmd --address http://10.0.0.7:9000 cluster peers

# Distributed reads + analytics — no secret needed.
bdscmd --address http://10.0.0.7:9000 cluster timeline
bdscmd --address http://10.0.0.7:9000 cluster count -d 1h
bdscmd --address http://10.0.0.7:9000 cluster search -q "kernel panic" -d 6h --limit 20
bdscmd --address http://10.0.0.7:9000 cluster knn -d 1h
bdscmd --address http://10.0.0.7:9000 cluster anomaly -d 1h
bdscmd --address http://10.0.0.7:9000 cluster denoise -d 1h --noise-threshold 0.5

# Sharded replicated writes (Phase 3) — no secret needed.
bdscmd --address http://10.0.0.7:9000 cluster add \
  -D '{"timestamp":1778000000,"key":"app.error","data":{"msg":"boom"}}'
bdscmd --address http://10.0.0.7:9000 cluster add-batch -f /path/to/batch.ndjson

# Fully-replicated stores (Phase 4) — no secret needed.
bdscmd --address http://10.0.0.7:9000 cluster doc-add \
  -m '{"title":"runbook"}' -c "Step 1: …"
bdscmd --address http://10.0.0.7:9000 cluster doc-delete -i <UUID>
bdscmd --address http://10.0.0.7:9000 cluster signal-emit -n disk-full -S critical
bdscmd --address http://10.0.0.7:9000 cluster script-add \
  -m '{"name":"hourly","schedule":"0 * * * *"}' -b "data 'x' results"
bdscmd --address http://10.0.0.7:9000 cluster script-delete -i <UUID>

# Phase 5 ops:
bdscmd --address http://10.0.0.7:9000 cluster hints     # per-peer backlog (no secret)
bdscmd --address http://10.0.0.7:9000 cluster sync      # force replay + AE (HMAC required)
```

**Hint backlog monitoring:**

```bash
# Phase 5 stats are surfaced in v2/cluster.peers (no auth):
curl -s -X POST http://10.0.0.7:9000 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"v2/cluster.peers","id":1,"params":{}}' \
  | jq '.result | {hint_backlog, tombstone_total, stats, peers: [.peers[] | {url, state, hints}]}'
```

**bdsweb:** the top-nav `Cluster` link goes to `/cluster`, which renders the
self-status tile, mode/threshold/replication tiles, and a peer table.
Disabled-cluster-mode shows a friendly explanation panel instead of an empty
table.

## 9. Failure modes (Phase 1 scope)

| Scenario | Behaviour |
|---|---|
| Bootstrap node down at startup | We try persisted peers next; if all fail, we run standalone. Gossip will reconnect when peers come back. |
| Slow peer | Per-peer RPC timeout (2 s default); after `suspect_timeout` of consecutive misses we mark Suspect, then Dead. |
| Embedding-model mismatch | `cluster.hello` rejects with `-32096`; the peer never enters the table. |
| Wrong shared secret | All v3/cluster.* calls reject with `-32098`; gossip logs the failure at debug level and retries on the next tick. |
| Network partition | Each side keeps gossiping with whatever peers it can still reach; on heal, gossip picks up the missing peers within a few ticks. |

## 10. Replication semantics (Phase 3)

`v3/add` and `v3/add.batch` write the record locally **synchronously**,
then dispatch fire-and-forget fan-out to `replication_factor - 1` random
Alive peers. The full lifecycle:

```
client → coordinator: v3/add { doc, replication_factor: N }
coordinator: write locally with shared UUIDv7      (sync)
coordinator → client: { id, replicas_dispatched: N-1, … }   (returns immediately)
coordinator: spawn detached task — for each replica:
    try v2/add (sync mode) with timeout cluster.peer_rpc_timeout
    on success → done
    on failure → enqueue hint in <dbpath>/network/hints.duckdb

every cluster.hint_replay_interval (default 10s):
    prune hints older than cluster.hint_max_age (default 24h)
    for each Alive peer with hints:
        drain up to 100, retry, delete on success
        first failure aborts that peer's batch (try again next tick)
```

The same UUIDv7 is used on every replica — that's how distributed reads
(v3/search, v3/knn, …) dedup the same record across peers. Caller-supplied
`doc.id` is preserved, making client-side retries idempotent (the
receiver's `(key, data_text)` dedup absorbs duplicates without creating
extra rows).

**Hint backlog telemetry** is exposed in three places:

- `v2/status.cluster.hint_backlog`
- `v2/cluster.peers.hint_backlog`
- bdsweb `/cluster` page (Phase 4 will add a per-peer breakdown)

## 11. Fully-replicated stores (Phase 4)

Phase 4 makes the docstore, signal store, and script store fully
replicated across every Alive peer. The configured
`cluster.full_replication_stores` (default `["docs", "signals", "scripts"]`)
controls which stores participate.

### 11.1 Real-time fan-out

Every v3/* mutating method follows the same pattern:

```text
client → coordinator: v3/<op> { … }
coordinator: stamp metadata.updated_at = now()  (for adds/updates)
coordinator: write locally with shared UUIDv7   (sync)
coordinator → client: { id, outcome: {peers_attempted, peers_succeeded, hints_queued}, … }   (returns immediately)
coordinator: spawn detached task — for EACH Alive peer:
    try v2/<op> with timeout cluster.peer_rpc_timeout
    on success → done
    on failure → enqueue hint in <dbpath>/network/hints.duckdb
```

The receiver-side `v2/*` methods accept an optional `id` field and are
**idempotent** for re-arrivals (already-present UUID → return
`existing: true` without re-writing), so retries and hint replays
converge to the right state.

### 11.2 Tombstoned deletes

`v3/doc.delete` and `v3/script.delete` write a tombstone to
`<dbpath>/network/tombstones.duckdb` keyed by `(store, id)` with a
shared `deleted_at` timestamp. The same `deleted_at` is propagated to
every peer's `v2/doc.delete` / `v2/script_delete`, so all replicas'
tombstone logs agree.

Tombstones are GC'd by the anti-entropy task after
`cluster.hint_max_age * 2` (default 48h) — long enough for any peer
that was down at delete time to recover via either hint replay or
anti-entropy diff.

### 11.3 Anti-entropy

A periodic background task (`cluster.antientropy_interval`, default
5 min) runs in the cluster background loop:

1. GC expired tombstones.
2. Pick a random Alive peer.
3. For each store in `cluster.full_replication_stores`:
   - Call `v2/<store>.list_ids` against the peer.
   - Diff remote `live[]` vs local; pull every UUID present remotely
     but not locally **and** not locally-tombstoned (skip resurrection).
   - Apply remote tombstones we don't have (delete + tombstone locally).

This catches up nodes that were down during a fan-out (and stayed down
past `hint_max_age`) and heals partition divergences.

**Phase 4 anti-entropy limitations:**

- **Updates are not re-replicated.** Anti-entropy only pulls **adds**
  and applies **tombstones**. If two peers' versions of the same UUID
  diverge (concurrent updates during partition), the divergence
  persists until the next live update. Real-time fan-out is
  last-write-wins-by-arrival-order; anti-entropy doesn't currently use
  `metadata.updated_at` for LWW pull-of-newer.
- **Signals anti-entropy is minimal.** Signal entries pulled by
  anti-entropy from a peer are a planned follow-up (the v2
  fetch-by-id surface for signals doesn't expose the full metadata in
  one call). In practice signals replicate via real-time fan-out and
  hint replay; missing signals are non-critical (they're append-only
  events).

## 12. What's not yet implemented

Phases 1–5 ship the full cluster surface — membership, distributed
reads, replicated writes (sharded + fully-replicated), and operational
tooling. Status:

| Phase | Status |
|---|---|
| 1 — Membership + discovery | ✅ shipped |
| 2 — Distributed reads + analytics fan-out | ✅ shipped |
| 3 — Sharded write replication (`v3/add` + hinted handoff) | ✅ shipped |
| 4 — Fully replicated stores + anti-entropy + tombstones | ✅ shipped |
| 5 — Operations: AE telemetry, `v3/cluster.sync`, distinct count, LWW updates, signal AE, dashboard polish | ✅ shipped |

**Resolved caveats** (called out by previous revisions of this doc, now
addressed):

- ~`v3/count` is approximate under replication.~ Phase 5 added
  `distinct: true` for accurate UUID-union counts.
- ~Update LWW not enforced.~ Phase 5 added `if_newer: true` on receiver
  v2 update methods and AE pull-newer.
- ~Signal anti-entropy is partial.~ Phase 5 added `v2/signal.get` and
  filled in the AE pull path.

**Remaining gaps** (acceptable for production; documented for clarity):

- **Wall-clock skew.** LWW uses Unix-second timestamps; clusters with
  unsynchronised clocks can favour the wrong side on concurrent
  partition updates. Run NTP.
- **Same-second update ties.** Two updates in the same Unix second on
  different coordinators are tied at `if_newer >`. A future revision
  could add a node-id tiebreaker.
- **No background re-replication of under-replicated sharded records.**
  When `v3/add` runs in `partial` mode and is later promoted to `full`,
  the under-replicated records stay at their lower replica count. AE
  doesn't apply to the sharded path.
- **Hint replay is per-peer ordered, not global.** Each peer's hints
  drain in seq order; inter-peer ordering is unbounded.
- **`cluster.max_fingerprints_per_peer` parsed but not enforced** on
  `v2/fingerprints.recent`.
- **No transport-level encryption.** Run TLS via a reverse proxy if
  peering across hostile networks. HMAC protects integrity and
  authenticity but not confidentiality.
