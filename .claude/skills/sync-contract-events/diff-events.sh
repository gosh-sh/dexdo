#!/usr/bin/env bash
# diff-events.sh — diff contract-ABI event signatures between two contract trees.
#
# Usage:
#   diff-events.sh <new-contracts-dir> [old-contracts-dir]
#
#   new-contracts-dir  directory with the UPDATED *.abi.json files (searched
#                      recursively), e.g. the acki-nacki compiled artifacts:
#                      /home/sauin/devel/gosh-sh/acki-nacki/contracts/<ver>_compiled/dex
#   old-contracts-dir  baseline tree; defaults to this repo's contracts/ dir.
#
# Output — one line per difference:
#   NEW      Kind.Event(type,type,...)    event exists only in the new tree
#   REMOVED  Kind.Event(type,type,...)    event exists only in the old tree
#   CHANGED  Kind.Event  old(sig) -> new(sig)   same name, different inputs
#
# Only contract kinds present in the NEW tree are compared, so pointing this at
# a dex-only update dir will not report every airegistry event as REMOVED.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
new_dir="${1:?usage: diff-events.sh <new-contracts-dir> [old-contracts-dir]}"
old_dir="${2:-$repo_root/contracts}"

[ -d "$new_dir" ] || { echo "error: not a directory: $new_dir" >&2; exit 1; }
[ -d "$old_dir" ] || { echo "error: not a directory: $old_dir" >&2; exit 1; }

# Emit "Kind.Event<TAB>type,type,..." for every event in every *.abi.json under $1.
list_events() {
    find "$1" -name '*.abi.json' -print0 | sort -z | while IFS= read -r -d '' f; do
        kind="$(basename "$f" .abi.json)"
        jq -r --arg k "$kind" \
            '(.events // [])[] | [$k + "." + .name, ([.inputs[].type] | join(","))] | @tsv' "$f"
    done | sort -u
}

new_events="$(list_events "$new_dir")"
# Restrict the baseline to contract kinds the new tree actually ships.
kinds="$(printf '%s\n' "$new_events" | cut -f1 | cut -d. -f1 | sort -u)"
old_events="$(list_events "$old_dir" | awk -F'\t' -v ks="$kinds" '
    BEGIN { n = split(ks, a, "\n"); for (i = 1; i <= n; i++) keep[a[i]] = 1 }
    { split($1, p, "."); if (p[1] in keep) print }')"

diff_found=0
while IFS=$'\t' read -r name new_sig; do
    [ -n "$name" ] || continue
    old_sig="$(printf '%s\n' "$old_events" | awk -F'\t' -v n="$name" '$1 == n { print $2 }')"
    if [ -z "$old_sig" ] && ! printf '%s\n' "$old_events" | cut -f1 | grep -qxF "$name"; then
        echo "NEW      $name($new_sig)"; diff_found=1
    elif [ "$old_sig" != "$new_sig" ]; then
        echo "CHANGED  $name  old($old_sig) -> new($new_sig)"; diff_found=1
    fi
done <<< "$new_events"

while IFS=$'\t' read -r name old_sig; do
    [ -n "$name" ] || continue
    if ! printf '%s\n' "$new_events" | cut -f1 | grep -qxF "$name"; then
        echo "REMOVED  $name($old_sig)"; diff_found=1
    fi
done <<< "$old_events"

[ "$diff_found" -eq 1 ] || echo "No event differences between $new_dir and $old_dir"
