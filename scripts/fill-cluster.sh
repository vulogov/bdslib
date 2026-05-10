#!/usr/bin/env bash
# scripts/fill-cluster.sh
#
# Populate a running 3-node bdsnode cluster with synthetic telemetry, logs,
# and docstore documents — same recipe as fill-store.sh, but uses the
# `bdscmd cluster *` family of subcommands so writes are replicated across
# the mesh.  Round-robins the coordinator across the configured nodes so
# every node originates some traffic.
#
# Usage:
#   ./scripts/fill-cluster.sh [OPTIONS]
#
# Options:
#   --addrs HOST:PORT,...   comma-separated cluster node addresses
#                           (default: http://127.0.0.1:9711,http://127.0.0.1:9712,http://127.0.0.1:9713
#                            — matches bds1.hjson / bds2.hjson / bds3.hjson)
#                           Each entry may use HOST:PORT or http://HOST:PORT form.
#   --config PATH           path to hjson config file used by bdscli
#                           (overrides BDS_CONFIG env var)
#   --tel-count N           telemetry records per key (default: 200)
#   --log-count N           log records per format (default: 300)
#   --doc-count N           docstore documents (default: 40); 0 skips docstore
#   --duration DUR          humantime duration override for the per-key /
#                           per-format / mixed lookback windows
#   --no-color              disable colour output
#
# Environment variables (lower precedence than flags):
#   BDSCMD_ADDRS            equivalent of --addrs
#   BDS_CONFIG              config file path used by bdscli
#   BDSCLI                  bdscli binary  (default: bdscli, fallback target/debug)
#   BDSCMD                  bdscmd binary  (default: bdscmd, fallback target/debug)
#
# Notes
# - Sharded telemetry/log writes go through `bdscmd cluster add-batch -f
#   <ndjson>` (which calls v3/add.batch on the receiving node).  Each
#   record is replicated to `cluster.replication_factor` peers
#   (configured in bds*.hjson).
# - Docstore writes go through `bdscmd cluster doc-add` — fully replicated
#   to every Alive peer.  No `cluster doc-add` batch form exists yet, so
#   docs are submitted one at a time.
# - The cluster fan-out is coordinator-driven: this script picks a
#   coordinator per batch and round-robins them, exercising every node as
#   originator at least once.

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────
BDSCLI="${BDSCLI:-bdscli}"
BDSCMD="${BDSCMD:-bdscmd}"
ADDRS_RAW="${BDSCMD_ADDRS:-http://127.0.0.1:9711,http://127.0.0.1:9712,http://127.0.0.1:9713}"
CONFIG_ARGS=()
TEL_COUNT=200
LOG_COUNT=300
DOC_COUNT=40
DURATION=""
COLOR=1

# ── Argument parsing ──────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case $1 in
        --addrs)     ADDRS_RAW="$2";              shift 2 ;;
        --config)    CONFIG_ARGS=(--config "$2"); shift 2 ;;
        --tel-count) TEL_COUNT="$2";              shift 2 ;;
        --log-count) LOG_COUNT="$2";              shift 2 ;;
        --doc-count) DOC_COUNT="$2";              shift 2 ;;
        --duration)  DURATION="$2";               shift 2 ;;
        --no-color)  COLOR=0;                     shift   ;;
        -h|--help)
            sed -n '3,42p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) printf 'Unknown option: %s\n' "$1" >&2; exit 1 ;;
    esac
done

# ── Colour helpers ────────────────────────────────────────────────────────────
if [[ $COLOR -eq 1 ]] && [[ -t 1 ]]; then
    _blue='\033[1;34m'; _green='\033[1;32m'; _red='\033[1;31m'
    _cyan='\033[1;36m'; _yellow='\033[0;33m'; _magenta='\033[1;35m'; _reset='\033[0m'
else
    _blue=''; _green=''; _red=''; _cyan=''; _yellow=''; _magenta=''; _reset=''
fi

info()  { printf "${_blue}[info]${_reset}  %s\n"   "$*"; }
ok()    { printf "${_green}[ ok ]${_reset}  %s\n"  "$*"; }
fail()  { printf "${_red}[fail]${_reset}  %s\n"    "$*" >&2; exit 1; }
step()  { printf "\n${_cyan}══ %s ══${_reset}\n"   "$*"; }
sub()   { printf "${_magenta}        coordinator → %s${_reset}\n" "$*"; }
tally() { printf "${_yellow}        %-30s %s${_reset}\n" "$1" "$2"; }

# ── Resolve binaries (fall back to cargo target dir) ─────────────────────────
resolve_bin() {
    local name="$1"
    if command -v "$name" &>/dev/null; then
        echo "$name"; return
    fi
    local cargo_bin
    cargo_bin="$(dirname "$0")/../target/debug/$name"
    if [[ -x "$cargo_bin" ]]; then
        echo "$cargo_bin"; return
    fi
    return 1
}

if ! BDSCLI=$(resolve_bin "$BDSCLI"); then
    fail "bdscli not found in PATH or target/debug/ — run 'cargo build --bin bdscli' first"
fi
if ! BDSCMD=$(resolve_bin "$BDSCMD"); then
    fail "bdscmd not found in PATH or target/debug/ — run 'cargo build --bin bdscmd' first"
fi

# ── Parse address list ────────────────────────────────────────────────────────
normalise_addr() {
    local a="$1"
    if [[ "$a" == http://* || "$a" == https://* ]]; then
        printf '%s' "$a"
    else
        printf 'http://%s' "$a"
    fi
}

ADDRS=()
IFS=',' read -ra _RAW <<< "$ADDRS_RAW"
for a in "${_RAW[@]}"; do
    [[ -z "$a" ]] && continue
    ADDRS+=("$(normalise_addr "$a")")
done
[[ "${#ADDRS[@]}" -ge 2 ]] || fail "expected at least 2 cluster nodes in --addrs (got ${#ADDRS[@]})"

# ── Preflight ─────────────────────────────────────────────────────────────────
step "Preflight"

if ! command -v jq &>/dev/null; then
    fail "jq not found — install it (brew install jq / apt install jq)"
fi
ok "jq found"
ok "bdscli: $BDSCLI"
ok "bdscmd: $BDSCMD"
info "Cluster nodes:"
for a in "${ADDRS[@]}"; do
    printf "        %s\n" "$a"
done

info "Reaching every node …"
for a in "${ADDRS[@]}"; do
    if ! "$BDSCMD" -a "$a" status &>/dev/null; then
        fail "bdsnode at $a is not reachable — start it first (see Documentation/CLUSTER.md)"
    fi
    nid=$("$BDSCMD" -a "$a" status 2>/dev/null | jq -r '.node_id // "unknown"')
    ok  "$a  (node_id=$nid)"
done

# Reuse the first node for read-only summary calls — peer-set is the same
# everywhere, so it doesn't matter which one we ask.
SUMMARY_ADDR="${ADDRS[0]}"

# Round-robin index — bumped after each batch so every node gets a turn as
# coordinator for replication writes.
RR=0
next_addr() {
    local a="${ADDRS[$(( RR % ${#ADDRS[@]} ))]}"
    RR=$(( RR + 1 ))
    printf '%s' "$a"
}

# Temp dir for NDJSON batches (cluster add-batch wants a file, not stdin).
TMPDIR_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/fill-cluster.XXXXXX")
trap 'rm -rf "$TMPDIR_ROOT"' EXIT

# ── Docstore (fully replicated — v3/doc.add) ─────────────────────────────────
doc_ok=0; doc_fail=0
if [[ "$DOC_COUNT" -eq 0 ]]; then
    step "Docstore — skipped (--doc-count=0)"
else
    step "Docstore — $DOC_COUNT documents (fully replicated, round-robin coordinator)"

    info "Generating and submitting …"
    while IFS= read -r line; do
        meta=$(printf '%s' "$line" | jq -c '.metadata' 2>/dev/null) || continue
        content=$(printf '%s' "$line" | jq -r '.content' 2>/dev/null)   || continue
        addr=$(next_addr)
        if "$BDSCMD" -a "$addr" cluster doc-add \
                --metadata "$meta" \
                --content  "$content" &>/dev/null; then
            (( doc_ok++  )) || true
        else
            (( doc_fail++ )) || true
        fi
    done < <("$BDSCLI" "${CONFIG_ARGS[@]}" generate docs --count "$DOC_COUNT" 2>/dev/null)

    tally "documents added:"  "$doc_ok"
    [[ $doc_fail -gt 0 ]] && tally "documents failed:" "$doc_fail"

    info "Rebuilding document vector index on every node …"
    for a in "${ADDRS[@]}"; do
        reindexed=$("$BDSCMD" -a "$a" doc-reindex 2>/dev/null | jq -r '.indexed // 0') || reindexed=0
        tally "$a re-indexed:" "$reindexed"
    done
    ok "Docstore done"
fi

# ── Telemetry (sharded replicated — v3/add.batch) ────────────────────────────
# Records go through cluster add-batch which calls v3/add.batch on the
# coordinator.  Each record is written locally and fan-out replicated
# (fire-and-forget + hints) to replication_factor-1 random Alive peers.
step "Telemetry — ${TEL_COUNT} records × 10 keys (sharded replicated)"

declare -A TEL_KEYS=(
    [cpu.usage]="6h"
    [mem.used_pct]="6h"
    [disk.io_wait]="3h"
    [disk.read_bytes]="3h"
    [net.rx_bytes]="12h"
    [net.tx_bytes]="12h"
    [http.latency_ms]="4h"
    [db.connections]="4h"
    [cache.hit_ratio]="2h"
    [queue.depth]="2h"
)

tel_total_n=0
for key in "${!TEL_KEYS[@]}"; do
    dur="${DURATION:-${TEL_KEYS[$key]}}"
    addr=$(next_addr)
    sub "$addr"
    ndjson="$TMPDIR_ROOT/tel-${key//./_}.ndjson"
    "$BDSCLI" "${CONFIG_ARGS[@]}" generate telemetry \
        --key      "$key" \
        --duration "$dur" \
        --count    "$TEL_COUNT" \
        2>/dev/null > "$ndjson"

    # cluster add-batch returns {ids:[…], n:…, replicas_dispatched:…}
    n=$("$BDSCMD" -a "$addr" cluster add-batch -f "$ndjson" 2>/dev/null \
         | jq -r '.n // 0') || n=0
    (( tel_total_n += n )) || true
    tally "$key ($dur):" "$n records (rf=$(jq -r '.replication_factor // "?"' < <("$BDSCMD" -a "$addr" cluster status 2>/dev/null) 2>/dev/null || printf '?'))"
done
ok "Telemetry done  (total ingested: $tel_total_n)"

# ── Logs (sharded replicated — v3/add.batch) ─────────────────────────────────
step "Logs — ${LOG_COUNT} records × 4 formats (sharded replicated)"

declare -A LOG_FORMATS=(
    [syslog]="24h"
    [http]="12h"
    [http-nginx]="12h"
    [traceback]="6h"
)

log_total_n=0
for fmt in "${!LOG_FORMATS[@]}"; do
    dur="${DURATION:-${LOG_FORMATS[$fmt]}}"
    addr=$(next_addr)
    sub "$addr"
    ndjson="$TMPDIR_ROOT/log-${fmt}.ndjson"
    "$BDSCLI" "${CONFIG_ARGS[@]}" generate log \
        --format   "$fmt" \
        --duration "$dur" \
        --count    "$LOG_COUNT" \
        2>/dev/null > "$ndjson"

    n=$("$BDSCMD" -a "$addr" cluster add-batch -f "$ndjson" 2>/dev/null \
         | jq -r '.n // 0') || n=0
    (( log_total_n += n )) || true
    tally "$fmt ($dur):" "$n records"
done
ok "Logs done  (total ingested: $log_total_n)"

# ── Mixed (telemetry + logs interleaved) ─────────────────────────────────────
step "Mixed batch (sharded replicated)"

MIXED_COUNT=$(( TEL_COUNT * 2 ))
MIXED_DUR="${DURATION:-8h}"
addr=$(next_addr)
sub "$addr"
ndjson="$TMPDIR_ROOT/mixed.ndjson"
"$BDSCLI" "${CONFIG_ARGS[@]}" generate mixed \
    --duration "$MIXED_DUR" \
    --count    "$MIXED_COUNT" \
    --ratio    0.5 \
    2>/dev/null > "$ndjson"

mixed_n=$("$BDSCMD" -a "$addr" cluster add-batch -f "$ndjson" 2>/dev/null \
           | jq -r '.n // 0') || mixed_n=0
tally "mixed ($MIXED_DUR, ratio=0.5):" "$mixed_n records"
ok "Mixed done"

# ── Cluster summary ──────────────────────────────────────────────────────────
# distinct=true gives the exact distinct-record count regardless of replication.
step "Cluster summary"

# Per-node counts so it's clear replication actually landed.
for a in "${ADDRS[@]}"; do
    n=$("$BDSCMD" -a "$a" count 2>/dev/null | jq -r '.count // "?"')
    tally "$a v2/count:" "$n"
done

# Cluster-wide counts via v3/count (sum vs distinct mode).
SUM_JSON=$("$BDSCMD" -a "$SUMMARY_ADDR" cluster count 2>/dev/null) || SUM_JSON='{}'
sum_count=$(printf '%s' "$SUM_JSON" | jq -r '.count // "?"')
tally "v3/count (sum):" "$sum_count"

# `bdscmd cluster count` doesn't expose --distinct yet; call v3/count directly.
DISTINCT_JSON=$(curl -fsS -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"v3/count","id":1,"params":{"distinct":true}}' \
    "$SUMMARY_ADDR" 2>/dev/null) || DISTINCT_JSON='{}'
distinct_count=$(printf '%s' "$DISTINCT_JSON" | jq -r '.result.count // "?"')
tally "v3/count distinct=true:" "$distinct_count"

TIMELINE_JSON=$("$BDSCMD" -a "$SUMMARY_ADDR" cluster timeline 2>/dev/null) || TIMELINE_JSON='{}'
min_ts=$(printf '%s' "$TIMELINE_JSON" | jq -r '.min_ts // "?"')
max_ts=$(printf '%s' "$TIMELINE_JSON" | jq -r '.max_ts // "?"')
min_human=$(  [[ "$min_ts" != "?" && "$min_ts" != "null" ]] && date -r "$min_ts" '+%Y-%m-%d %H:%M:%S %Z' 2>/dev/null || echo "$min_ts")
max_human=$(  [[ "$max_ts" != "?" && "$max_ts" != "null" ]] && date -r "$max_ts" '+%Y-%m-%d %H:%M:%S %Z' 2>/dev/null || echo "$max_ts")
tally "oldest event (cluster):" "$min_human"
tally "newest event (cluster):" "$max_human"
tally "docstore documents added:" "$doc_ok"

# Hint backlog quick-look so the operator can spot any failed fan-outs.
PEERS_JSON=$(curl -fsS -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"v2/cluster.peers","id":1,"params":{}}' \
    "$SUMMARY_ADDR" 2>/dev/null) || PEERS_JSON='{}'
backlog=$(printf '%s' "$PEERS_JSON" | jq -r '.result.hint_backlog // 0')
tally "hint backlog (originator):" "$backlog"

printf "\n${_green}Done.${_reset}\n"
if [[ "$backlog" -gt 0 ]]; then
    printf "${_yellow}        Note: %s hints still queued — replication didn't reach every peer.${_reset}\n" "$backlog"
    printf "${_yellow}        Wait for the periodic replay (cluster.hint_replay_interval), or force it:${_reset}\n"
    printf "${_yellow}              BDSCMD_CLUSTER_SECRET=… %s -a %s cluster sync${_reset}\n" "$BDSCMD" "$SUMMARY_ADDR"
fi
