#!/usr/bin/env bash
# scripts/load_internal_documentation.sh
#
# Re-ingest every Markdown / text file under ./Documentation/ into the
# running bdsnode docstore, tagging each document with
# `metadata.internal_doc = true` so subsequent runs can clean up only
# their own previous output without touching unrelated documents.
#
# Workflow:
#   1. Health check the bdsnode RPC endpoint.
#   2. Enumerate every live doc UUID via v2/doc.list_ids.
#   3. For each live doc, fetch metadata; delete iff `internal_doc=true`.
#   4. Walk ./Documentation/ for *.md / *.txt; doc-add each one with
#      { internal_doc: true, name, path, source, ingested_at }.
#   5. Rebuild the HNSW vector index (doc-reindex).
#
# The script is idempotent: re-running deletes the previous batch and
# re-loads from the current tree.  Documents NOT tagged internal_doc=true
# (user-loaded knowledge base entries, doc-add-file chunks from other
# scripts, etc.) are left alone.
#
# Usage:
#   ./scripts/load_internal_documentation.sh [OPTIONS]
#
# Options:
#   --addr HOST:PORT      bdsnode address (default: http://127.0.0.1:9000)
#                         Accepts both "HOST:PORT" and "http://HOST:PORT".
#   --doc-dir PATH        documentation root (default: ./Documentation)
#   --include-ext EXT     extra file extension to ingest, e.g. ".rst"
#                         May be repeated; default set is ".md .txt".
#   --dry-run             show what would be deleted / added without
#                         contacting the server for any mutations
#                         (still does the list_ids + metadata reads)
#   --no-color            disable colour output
#   -h, --help            this message
#
# Environment variables (lower precedence than flags):
#   BDSCMD_ADDR           equivalent of --addr
#   BDSCMD                bdscmd binary (default: bdscmd)
#
# Required tools: bdscmd, curl, jq, find

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────
BDSCMD="${BDSCMD:-bdscmd}"
ADDR="${BDSCMD_ADDR:-http://127.0.0.1:9000}"
DOC_DIR="./Documentation"
EXTRA_EXTS=()
DRY_RUN=0
COLOR=1

# ── Argument parsing ──────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case $1 in
        --addr)        ADDR="$2";              shift 2 ;;
        --doc-dir)     DOC_DIR="$2";           shift 2 ;;
        --include-ext) EXTRA_EXTS+=("$2");     shift 2 ;;
        --dry-run)     DRY_RUN=1;              shift   ;;
        --no-color)    COLOR=0;                shift   ;;
        -h|--help)     sed -n '2,/^set -euo/p' "$0" | sed -e 's/^# \{0,1\}//' -e '$d'; exit 0 ;;
        *)             echo "unknown arg: $1" >&2; exit 64 ;;
    esac
done

# ── Colour helpers (same palette as fill-store.sh) ────────────────────────────
if [[ $COLOR -eq 1 ]] && [[ -t 1 ]]; then
    _blue='\033[1;34m'; _green='\033[1;32m'; _red='\033[1;31m'
    _cyan='\033[1;36m'; _yellow='\033[0;33m'; _grey='\033[0;90m'; _reset='\033[0m'
else
    _blue=''; _green=''; _red=''; _cyan=''; _yellow=''; _grey=''; _reset=''
fi
info()  { printf "${_blue}[info]${_reset}  %s\n"   "$*"; }
ok()    { printf "${_green}[ ok ]${_reset}  %s\n"  "$*"; }
warn()  { printf "${_yellow}[warn]${_reset}  %s\n" "$*" >&2; }
fail()  { printf "${_red}[fail]${_reset}  %s\n"    "$*" >&2; exit 1; }
step()  { printf "\n${_cyan}══ %s ══${_reset}\n"   "$*"; }
tally() { printf "${_yellow}        %-30s %s${_reset}\n" "$1" "$2"; }
dim()   { printf "${_grey}        %s${_reset}\n"  "$*"; }

# ── Resolve bdscmd (PATH → target/debug/) ─────────────────────────────────────
resolve_bin() {
    local name="$1"
    if command -v "$name" &>/dev/null; then echo "$name"; return; fi
    local cargo_bin
    cargo_bin="$(dirname "$0")/../target/debug/$name"
    if [[ -x "$cargo_bin" ]]; then echo "$cargo_bin"; return; fi
    return 1
}
if ! BDSCMD=$(resolve_bin "$BDSCMD"); then
    fail "bdscmd not found in PATH or target/debug/ — run 'cargo build --bin bdscmd' first"
fi

# ── URL normalisation ─────────────────────────────────────────────────────────
case "$ADDR" in
    http://*|https://*) ;;
    *) ADDR="http://$ADDR" ;;
esac

BDSCMD_OPTS=(-a "$ADDR")

# ── Preflight ─────────────────────────────────────────────────────────────────
step "Preflight"

for tool in curl jq find; do
    command -v "$tool" >/dev/null 2>&1 || fail "$tool not found in PATH"
done
ok "tools: bdscmd jq curl find"

[[ -d "$DOC_DIR" ]] || fail "doc dir not found: $DOC_DIR"
ok "doc dir: $DOC_DIR"

info "Connecting to bdsnode at $ADDR …"
NODE_ID=$("$BDSCMD" "${BDSCMD_OPTS[@]}" -r status 2>/dev/null | jq -r '.node_id // empty') \
    || fail "bdscmd status failed — is bdsnode running at $ADDR?"
[[ -n "$NODE_ID" ]] || fail "bdsnode unreachable at $ADDR"
ok "bdsnode up  (node_id=$NODE_ID)"

[[ $DRY_RUN -eq 1 ]] && warn "dry-run mode — no doc-delete / doc-add / doc-reindex will be sent"

# ── Step 1: enumerate live documents ──────────────────────────────────────────
#
# bdscmd does not expose v2/doc.list_ids (it's an anti-entropy receiver,
# not part of the surface user-facing CLI). We hit it directly via curl;
# everything else in this script goes through bdscmd as the user asked.
step "Enumerating live documents"

list_payload='{"jsonrpc":"2.0","method":"v2/doc.list_ids","id":1,"params":{}}'
ids_json=$(curl -sS -X POST "$ADDR/" \
    -H 'Content-Type: application/json' \
    -d "$list_payload") || fail "v2/doc.list_ids call failed"

n_live=$(echo "$ids_json" | jq -r '.result.n_live // 0')
ok "found $n_live live doc(s)"

# ── Step 2: delete previous internal docs ─────────────────────────────────────
step "Removing previous internal documentation"
checked=0; deleted=0; skipped=0; del_fail=0

if [[ "$n_live" -gt 0 ]]; then
    while IFS= read -r doc_id; do
        ((checked++)) || true
        meta=$("$BDSCMD" "${BDSCMD_OPTS[@]}" -r doc-get-metadata --id "$doc_id" 2>/dev/null) || {
            warn "could not fetch metadata for $doc_id — skipping"
            continue
        }
        # `internal_doc` lives directly under `metadata`. Default false
        # so unrelated docs (no flag) are never matched.
        is_internal=$(echo "$meta" | jq -r '.metadata.internal_doc // false')
        if [[ "$is_internal" != "true" ]]; then
            ((skipped++)) || true
            continue
        fi
        if [[ $DRY_RUN -eq 1 ]]; then
            name=$(echo "$meta" | jq -r '.metadata.name // .metadata.path // "<no name>"')
            dim "would delete  $doc_id  ($name)"
            ((deleted++)) || true
            continue
        fi
        if "$BDSCMD" "${BDSCMD_OPTS[@]}" doc-delete --id "$doc_id" >/dev/null 2>&1; then
            ((deleted++)) || true
        else
            ((del_fail++)) || true
            warn "doc-delete failed for $doc_id"
        fi
    done < <(echo "$ids_json" | jq -r '.result.live[].id')
fi

tally "checked:"  "$checked"
tally "deleted:"  "$deleted"
tally "skipped (not internal):"  "$skipped"
[[ $del_fail -gt 0 ]] && tally "delete errors:" "$del_fail"

# ── Step 3: load every Markdown / text file under Documentation/ ──────────────
step "Loading documentation from $DOC_DIR"

# Build find expressions: default *.md + *.txt, plus any --include-ext extras.
find_args=( "$DOC_DIR" -type f \( -name '*.md' -o -name '*.txt' )
for ext in "${EXTRA_EXTS[@]}"; do
    # Accept ".rst" or "rst" — normalise to "*.rst".
    case "$ext" in
        .*) pattern="*$ext" ;;
        *)  pattern="*.$ext" ;;
    esac
    find_args+=( -o -name "$pattern" )
done
find_args+=( \) -print0 )

now_ts=$(date +%s)
added=0; add_fail=0; total=0

while IFS= read -r -d '' file; do
    ((total++)) || true
    rel="${file#./}"
    name=$(basename "$file")

    # Build metadata via jq so JSON encoding (quotes, slashes, newlines)
    # is correct regardless of filename / path content.
    meta=$(jq -nc \
        --arg name   "$name" \
        --arg path   "$rel" \
        --arg source "load_internal_documentation.sh" \
        --argjson ts  "$now_ts" \
        '{ internal_doc: true, name: $name, path: $path, source: $source, ingested_at: $ts }')

    if [[ $DRY_RUN -eq 1 ]]; then
        dim "would add     $rel"
        ((added++)) || true
        continue
    fi

    # Content goes through bash command substitution.  Markdown files in
    # this repo top out around 150 KB which is well under ARG_MAX on
    # every supported platform (~256 KB on macOS, ~2 MB on Linux), so
    # there's no need for stdin/file plumbing.  Command substitution
    # strips a single trailing newline; bdslib's docstore doesn't care.
    #
    # The `--key=value` form (not `--key value`) is mandatory here —
    # some files begin with "- " or "-#" which clap's space-separated
    # form would otherwise mistake for a flag.
    if "$BDSCMD" "${BDSCMD_OPTS[@]}" doc-add \
        --metadata="$meta" \
        --content="$(cat "$file")" >/dev/null 2>&1; then
        ((added++)) || true
    else
        ((add_fail++)) || true
        warn "doc-add failed for $rel"
    fi
done < <(find "${find_args[@]}")

tally "scanned:" "$total"
tally "added:"   "$added"
[[ $add_fail -gt 0 ]] && tally "add errors:" "$add_fail"

# ── Step 4: rebuild the vector index ──────────────────────────────────────────
step "Rebuilding document vector index"
if [[ $DRY_RUN -eq 1 ]]; then
    dim "would call doc-reindex"
else
    indexed=$("$BDSCMD" "${BDSCMD_OPTS[@]}" -r doc-reindex 2>/dev/null | jq -r '.indexed // 0') \
        || indexed=0
    tally "documents re-indexed:" "$indexed"
fi

ok "done"
