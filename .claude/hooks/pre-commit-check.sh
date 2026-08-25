#!/usr/bin/env bash
# PreToolUse hook (Bash matcher, if: "Bash(git *)"): before a `git commit`,
# verify fmt and clippy so the forgot-fmt → fix → recommit loop never starts.
# Exit 2 blocks the commit; stderr is fed back to Claude.
#
# FAIL CLOSED. Every failure below exits 2, including the ones that are the hook's
# own fault (no jq, unreadable payload, unresolvable project directory). A gate
# whose only job is to block has exactly one dangerous direction, and `|| exit 0`
# points that way: it lets an unchecked commit through and says nothing, which is
# indistinguishable from a clean run. Exiting 2 with a reason costs a visible
# error on a broken setup and cannot cost a silently unverified commit.
set -u
if ! cmd=$(jq -r '.tool_input.command // empty' 2>/dev/null); then
  echo "commit gate blocked: cannot read the tool payload (is jq installed?). Failing closed." >&2
  exit 2
fi
# Not a commit — nothing to gate. This is the ONLY clean exit before the checks.
grep -qE '(^|[;&|[:space:]])git([[:space:]]+-C[[:space:]]+[^[:space:]]+)?[[:space:]]+commit' <<<"$cmd" || exit 0
if ! cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}"; then
  echo "commit blocked: cannot enter the project directory, so fmt/clippy cannot run." >&2
  exit 2
fi
if ! out=$(cargo +nightly fmt --all -- --check 2>&1); then
  { echo "commit blocked: cargo fmt --check failed — run 'make fmt', re-stage, retry."
    printf '%s\n' "$out" | head -20; } >&2
  exit 2
fi
if ! out=$(cargo clippy --workspace --all-targets --no-deps -- -D warnings 2>&1); then
  { echo "commit blocked: clippy failed — fix the warnings, then retry."
    printf '%s\n' "$out" | tail -30; } >&2
  exit 2
fi
exit 0
