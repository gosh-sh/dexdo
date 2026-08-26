#!/usr/bin/env bash
# diff-signatures.sh — diff Solidity FUNCTION SIGNATURES INCLUDING MODIFIERS
# between two revisions of the contracts tree.
#
# Usage:
#   diff-signatures.sh <new-rev> [old-rev] [contracts-dir]
#
#   new-rev   git revision with the updated contracts (e.g. HEAD, or the sync commit)
#   old-rev   baseline revision; defaults to <new-rev>~1
#   dir       contracts subtree; defaults to `contracts`
#
# Why this exists: `diff-events.sh` compares ABI event sets, and an ABI diff
# compares parameter names/types. NEITHER sees a change to a function's
# modifiers, its owner gate, or its state mutability — those do not reach the
# ABI at all. v4.0.32 shipped exactly that shape of change:
#
#   - PrivateNote.changeOwner  gained  onlyOwnerPubkey(_ephemeralPubkey)
#     (before it had no owner gate — an unsigned call went through)
#   - OracleEventList.resolveRange / onWeeklyMedian  became  `view`
#   - PrivateNote.onInferencePlaced / onInferenceFilled  became `view`,
#     dropping the `accept` modifier for an explicit `tvm.accept()`
#
# Output is a plain diff: `<` lines are the old signature, `>` the new one.
# A pair with the same name and different tail is a modifier/mutability change.
#
# NOTE on `view` in TVM Solidity: it means "does not write state". Emitting
# events and sending outbound messages from a `view` function is legal and
# still happens, so a `view` marking is not by itself a behavioural break —
# read the body before raising an alarm.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
new_rev="${1:?usage: diff-signatures.sh <new-rev> [old-rev] [contracts-dir]}"
old_rev="${2:-${new_rev}~1}"
dir="${3:-contracts}"

cd "$repo_root"

# Both revisions must resolve, and the directory must exist in BOTH of them.
# `git ls-tree` on a path that is not in the tree exits 0 with no output, so
# without these guards a typo in <contracts-dir> makes `sigs` emit nothing for
# both sides — an empty diff, and the all-clear message below. That is the exact
# conclusion this script exists to disprove, printed with total confidence.
# `set -e` cannot help: the failure is inside a process substitution, whose exit
# status the shell never inspects.
for rev in "$old_rev" "$new_rev"; do
    git rev-parse --verify --quiet "${rev}^{commit}" >/dev/null \
        || { echo "error: not a commit: $rev" >&2; exit 1; }
    git ls-tree -r --name-only "$rev" -- "$dir" | grep -q '\.sol$' \
        || { echo "error: no .sol files under '$dir' at $rev" >&2; exit 1; }
done

# Emit "Kind . <signature>" for every function in every .sol file at $1.
# Comments are stripped first so a `function` word inside a doc block cannot
# be mistaken for a declaration.
sigs() {
    local rev="$1"
    git ls-tree -r --name-only "$rev" -- "$dir" | grep '\.sol$' | sort | while read -r f; do
        local base
        base="$(basename "$f" .sol)"
        git show "$rev:$f" \
            | sed -E 's,//.*$,,' \
            | perl -0777 -pe 's,/\*.*?\*/,,gs' \
            | tr '\n' ' ' \
            | grep -oE '(function [A-Za-z_][A-Za-z0-9_]*|constructor|receive|fallback|onBounce)[^;{}]*\{' \
            | sed -E 's/\s+/ /g; s/ \{$//' \
            | sed "s|^|${base} . |"
    done
}

# Captured, not piped straight into `||`. `diff` exits 1 when it finds
# differences, and under `set -o pipefail` that 1 becomes the PIPELINE's status
# even though `grep` succeeded — so an `|| { echo "no differences"; }` tail fired
# on every run that had something to report, printing the all-clear directly
# underneath the differences it had just listed. `|| true` keeps the empty case
# (grep exits 1, nothing matched) from tripping `set -e`.
changes="$(diff <(sigs "$old_rev") <(sigs "$new_rev") | grep -E '^[<>]' || true)"

if [ -n "$changes" ]; then
    printf '%s\n' "$changes"
else
    echo "No function-signature differences between $old_rev and $new_rev"
fi
