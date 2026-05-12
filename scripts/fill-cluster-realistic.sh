#!/usr/bin/env bash
# scripts/fill-cluster-realistic.sh
#
# Populate a running 3-node bdsnode cluster with a *realistic* synthetic
# corpus — structured incident scenarios, background noise, and rare
# anomalies — using the new `bdscli generate realistic` command.  Suitable
# for emulating Root Cause Analysis (RCA), anomaly detection, denoising,
# k-NN clustering, drain3 template mining, and aggregated search.
#
# Drop-in companion to fill-cluster.sh: same flags + cluster fan-out
# recipe, but the data is engineered rather than purely random — every
# scenario is a precursor → failure → consequence chain across multiple
# keys with consistent host/region/env tags so RCA can cluster them and
# detect lead/lag correctly.
#
# Usage:
#   ./scripts/fill-cluster-realistic.sh [OPTIONS]
#
# Options:
#   --addrs HOST:PORT,...   comma-separated cluster node addresses
#                           (default: http://127.0.0.1:9711,http://127.0.0.1:9712,http://127.0.0.1:9713
#                            — matches bds1.hjson / bds2.hjson / bds3.hjson)
#                           Each entry may use HOST:PORT or http://HOST:PORT form.
#   --config PATH           path to hjson config file used by bdscli
#                           (overrides BDS_CONFIG env var)
#   --total N               approximate total records per batch (default: 2000).
#                           Actual output is somewhat higher because each
#                           scenario emits 30–60 records on top.
#   --scenarios N           number of incident scenarios per batch (default: 3).
#                           Each picks an archetype: DB overload, OOM kill,
#                           TLS cert expiry, deployment regression, network
#                           partition.
#   --noise-ratio F         fraction of --total reserved for background
#                           noise.  Clamped to [0.0, 0.95].  Default 0.7.
#   --anomaly-ratio F       fraction reserved for rare anomalies.  Clamped
#                           to [0.0, 0.10].  Default 0.02.
#   --batches N             how many separate realistic batches to submit,
#                           one per coordinator.  Default = number of nodes
#                           so every node originates at least one batch.
#   --duration DUR          humantime duration window (default: 6h)
#   --doc-count N           docstore documents (default: 40); 0 skips docstore.
#                           Same recipe as fill-cluster.sh — operational
#                           runbooks / postmortems / KB articles.
#   --seed-base N           base RNG seed.  Each batch uses seed-base + i.
#                           Useful for reproducible test runs.  Default:
#                           random (non-deterministic).
#   --no-color              disable colour output
#
# Environment variables (lower precedence than flags):
#   BDSCMD_ADDRS            equivalent of --addrs
#   BDS_CONFIG              config file path used by bdscli
#   BDSCLI                  bdscli binary  (default: bdscli, fallback target/debug)
#   BDSCMD                  bdscmd binary  (default: bdscmd, fallback target/debug)
#
# Notes
# - Each realistic batch produces a self-contained set of scenarios.
#   With --batches=3 (the default for a 3-node cluster) you get ~9
#   incident scenarios distributed across the timeline — enough to
#   exercise RCA, k-NN clustering, and template mining at the same time.
# - Telemetry/log writes go through `bdscmd cluster add-batch -f <ndjson>`
#   which calls v3/add.batch.  Each record is replicated to
#   replication_factor peers (configured in bds*.hjson).
# - Docstore writes go through `bdscmd cluster doc-add` — fully replicated.

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────
BDSCLI="${BDSCLI:-bdscli}"
BDSCMD="${BDSCMD:-bdscmd}"
ADDRS_RAW="${BDSCMD_ADDRS:-http://127.0.0.1:9711,http://127.0.0.1:9712,http://127.0.0.1:9713}"
CONFIG_ARGS=()
TOTAL=2000
SCENARIOS=3
NOISE_RATIO=0.7
ANOMALY_RATIO=0.02
BATCHES=""
DURATION="6h"
DOC_COUNT=40
SEED_BASE=""
COLOR=1

# ── Argument parsing ──────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case $1 in
        --addrs)          ADDRS_RAW="$2";              shift 2 ;;
        --config)         CONFIG_ARGS=(--config "$2"); shift 2 ;;
        --total)          TOTAL="$2";                  shift 2 ;;
        --scenarios)      SCENARIOS="$2";              shift 2 ;;
        --noise-ratio)    NOISE_RATIO="$2";            shift 2 ;;
        --anomaly-ratio)  ANOMALY_RATIO="$2";          shift 2 ;;
        --batches)        BATCHES="$2";                shift 2 ;;
        --duration)       DURATION="$2";               shift 2 ;;
        --doc-count)      DOC_COUNT="$2";              shift 2 ;;
        --seed-base)      SEED_BASE="$2";              shift 2 ;;
        --no-color)       COLOR=0;                     shift   ;;
        -h|--help)
            sed -n '3,68p' "$0" | sed 's/^# \{0,1\}//'
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

# Default --batches to the node count if the operator didn't pin it,
# so every node originates at least one realistic batch as coordinator.
if [[ -z "$BATCHES" ]]; then
    BATCHES="${#ADDRS[@]}"
fi

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
info "Realistic config:"
tally "total per batch:"     "$TOTAL"
tally "scenarios per batch:" "$SCENARIOS"
tally "noise ratio:"         "$NOISE_RATIO"
tally "anomaly ratio:"       "$ANOMALY_RATIO"
tally "duration window:"     "$DURATION"
tally "batches:"             "$BATCHES"
[[ -n "$SEED_BASE" ]] && tally "seed-base:" "$SEED_BASE (reproducible)"
[[ -z "$SEED_BASE" ]] && tally "seed-base:" "(random — non-deterministic)"

info "Reaching every node …"
for a in "${ADDRS[@]}"; do
    if ! "$BDSCMD" -a "$a" status &>/dev/null; then
        fail "bdsnode at $a is not reachable — start it first (see Documentation/CLUSTER.md)"
    fi
    nid=$("$BDSCMD" -a "$a" status 2>/dev/null | jq -r '.node_id // "unknown"')
    ok  "$a  (node_id=$nid)"
done

# Reuse the first node for read-only summary calls.
SUMMARY_ADDR="${ADDRS[0]}"

# Round-robin index — bumped after each batch so every node gets a turn
# as coordinator for replication writes.
RR=0
next_addr() {
    local a="${ADDRS[$(( RR % ${#ADDRS[@]} ))]}"
    RR=$(( RR + 1 ))
    printf '%s' "$a"
}

# Temp dir for NDJSON batches.
TMPDIR_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/fill-cluster-realistic.XXXXXX")
trap 'rm -rf "$TMPDIR_ROOT"' EXIT

# ── Docstore (fully replicated) ──────────────────────────────────────────────
# Same recipe as fill-cluster.sh — operational runbooks / postmortems /
# KB articles.  Important for aggregated-search "Analyze this!" because
# the LLM cross-references telemetry + matched documents.
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

# ── Realistic batches (sharded replicated — v3/add.batch) ────────────────────
step "Realistic batches — ${BATCHES} × (~${TOTAL} records + ${SCENARIOS} scenarios)"

total_ingested=0
total_failed=0
for ((i=0; i<BATCHES; i++)); do
    addr=$(next_addr)
    sub "$addr"

    # Build the bdscli invocation.  Seed only when --seed-base supplied;
    # each batch gets seed-base + i so the multi-batch run stays
    # reproducible while still producing distinct scenario placements.
    seed_args=()
    if [[ -n "$SEED_BASE" ]]; then
        seed_args=(--seed "$(( SEED_BASE + i ))")
    fi

    ndjson="$TMPDIR_ROOT/realistic-$i.ndjson"
    if ! "$BDSCLI" "${CONFIG_ARGS[@]}" generate realistic \
            --total         "$TOTAL" \
            --scenarios     "$SCENARIOS" \
            --noise-ratio   "$NOISE_RATIO" \
            --anomaly-ratio "$ANOMALY_RATIO" \
            --duration      "$DURATION" \
            "${seed_args[@]}" \
            > "$ndjson" 2>/dev/null
    then
        (( total_failed++ )) || true
        tally "batch $i (generate failed):" "skipped"
        continue
    fi

    # Show what we asked for — actual output is somewhat higher than
    # --total because scenarios pile records on top.
    line_count=$(wc -l < "$ndjson" | tr -d ' ')

    n=$("$BDSCMD" -a "$addr" cluster add-batch -f "$ndjson" 2>/dev/null \
         | jq -r '.n // 0') || n=0
    (( total_ingested += n )) || true

    if [[ -n "$SEED_BASE" ]]; then
        tally "batch $i (seed=$(( SEED_BASE + i ))):" "$n records (generated $line_count)"
    else
        tally "batch $i:" "$n records (generated $line_count)"
    fi
done

ok "Realistic batches done  (total ingested: $total_ingested)"

# ── Cluster summary ──────────────────────────────────────────────────────────
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

PEERS_JSON=$(curl -fsS -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"v2/cluster.peers","id":1,"params":{}}' \
    "$SUMMARY_ADDR" 2>/dev/null) || PEERS_JSON='{}'
backlog=$(printf '%s' "$PEERS_JSON" | jq -r '.result.hint_backlog // 0')
tally "hint backlog (originator):" "$backlog"

# ── Hint about what to explore next ──────────────────────────────────────────
printf "\n${_green}Done.${_reset}\n"
if [[ "$backlog" -gt 0 ]]; then
    printf "${_yellow}        Note: %s hints still queued — replication didn't reach every peer.${_reset}\n" "$backlog"
    printf "${_yellow}        Wait for the periodic replay (cluster.hint_replay_interval), or force it:${_reset}\n"
    printf "${_yellow}              BDSCMD_CLUSTER_SECRET=… %s -a %s cluster sync${_reset}\n" "$BDSCMD" "$SUMMARY_ADDR"
fi

echo ""
echo "Try the LLM analyses on bdsweb against the data we just emitted:"
echo "  Analysis → Detect anomalies   (kernel soft-lockups, ECC errors, novel exceptions)"
echo "  Analysis → Denoise             (kept vs filter-floor — heartbeats should drop out)"
echo "  Analysis → k-NN analysis       (incident records cluster; routine traffic forms its own cluster)"
echo "  RCA → Telemetry RCA            (try failure_key=service.crashed / cert.expired / deployment.rollback)"
echo "  RCA → Templates RCA            (drain3 templates from the cascade log lines)"
echo "  Telemetry → Templates summary  (story-from-summary across mined drain3 patterns)"
