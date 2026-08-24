#!/usr/bin/env bash
# The observer: end-to-end indexer invariants over the whole run. It creates no
# traffic and reads only the deployed database.
#
# Required env: DEXDO_SHA, PIPELINE_ID, TEST_DATABASE_URL, E2E_ELAPSED_SECONDS.
# Optional: DEXDO_REPO, DEXDO_DIR, OBSERVER_DEADLINE_SECS.
#
# Piped over ssh from the CI runner's checkout, like sdk-proof-on-host.sh and for the
# same reason: the shared checkout on host B is exactly the state this script is about
# to overwrite, and it may hold anything from a previous pipeline. The lease protocol
# is inlined for the same reason rather than sourced from the host's disk.
set -euo pipefail

# NAME THE FAILURE, ALWAYS. Every explicit refusal below prints a FATAL and its
# own exit code, but the statements between them had no voice: a command that
# dies on its own leaves `set -e` to end the script silently, and the step log
# stops mid-sentence with a bare number in Woodpecker.
#
# Added after pipeline #300, where this script exited 126 twenty-two seconds in
# and said nothing. The cause turned out NOT to be this script — the run had
# overrun the repository's 120-minute timeout and Woodpecker was killing it, so
# 126 was the shape of the interruption rather than a fault here. That is exactly
# why the trap is worth having: two wrong diagnoses were argued from the absence
# of a message. A trap cannot report a SIGKILL, but it names everything short of
# one, which is the difference between reading a log and guessing at it.
#
# `$BASH_COMMAND` is the command about to run, `$LINENO` its line. Printed to
# stderr so it survives even when stdout is being consumed.
trap 'rc=$?; echo "FATAL: line $LINENO exited $rc while running: $BASH_COMMAND" >&2' ERR

: "${DEXDO_SHA:?}" "${PIPELINE_ID:?}" "${TEST_DATABASE_URL:?}" "${E2E_ELAPSED_SECONDS:?}"

case "$PIPELINE_ID" in
  *[[:space:]]*) echo "FATAL: PIPELINE_ID must not contain whitespace: '$PIPELINE_ID'"; exit 64 ;;
esac

# The run's start IN THIS HOST'S CLOCK, computed FIRST of all — before the sync.
# `E2E_ELAPSED_SECONDS` is measured entirely against the CI runner's clock (the
# difference of two of its own readings), so the runner-to-host clock offset does not
# enter the result; the mark is compared against `raw_events.created_at`, which is set
# by this same host's Postgres. Taking `date` after `git fetch`/`checkout` would push
# the run's start forward by the sync duration — seconds on a warm host, minutes on a
# cold clone. The direction is safe (a narrower window, no false red), but it is
# exactly the offset removed here by one line.
E2E_STARTED_AT=$(( $(date +%s) - E2E_ELAPSED_SECONDS ))
export E2E_STARTED_AT TEST_DATABASE_URL

REPO="${DEXDO_REPO:-https://github.com/gosh-sh/dexdo.git}"
DIR="${DEXDO_DIR:-$HOME/dexdo-e2e}"
# shellcheck disable=SC1091  # host-specific path; rustup puts it there on this host's own prior setup
source "$HOME/.cargo/env"

GUARD=/var/lock/dexdo-e2e.lock
LEASE=/var/lock/dexdo-e2e.lease

# The ownership check is hard, never `|| true`. The observer reads the stand's
# database; if the lease is no longer ours, the stand belongs to another pipeline and
# its database has nothing to do with our run. Every refusal exits through an explicit
# `exit` — for the same reason as in the template (sdk-proof-on-host.sh): bash drops
# `errexit` inside the left-hand side of `||`, so a failure through it would not stop
# the function.
# THE RUN'S TIME BUDGET IS AN INPUT TO CORRECTNESS, NOT A BACKGROUND FACT.
# Woodpecker kills the whole pipeline at the repository's timeout (120 min as of
# 2026-08-24). Steps still queued at that moment do not run — including
# `net_down`, which is what releases the host lease. So a run that overruns does
# not merely fail: it leaves the lease held for its full 90-minute TTL and every
# subsequent pipeline dies on `lease_acquire` in two seconds. That happened in
# #300 -> #301.
#
# This script is last in the DAG, so it is the step most likely to be cut off. It
# prints its own wall clock at the end for exactly that reason — a run that ends
# without that line ended by force, not by choice.
lease_assert() {
  exec 9>"$GUARD"
  flock -w 30 9 || { echo "FATAL: guard busy (flock on $GUARD not acquired within 30s)"; exit 70; }
  if [ -f "$LEASE" ]; then
    read -r L_ID _ < "$LEASE"
  else
    L_ID=""
  fi
  [ "$L_ID" = "$PIPELINE_ID" ] || { echo "FATAL: lease is not ours ($L_ID != $PIPELINE_ID)"; exit 72; }
  echo "$PIPELINE_ID $(date +%s)" > "$LEASE"
  exec 9>&-
}

lease_assert                                   # before first touching the shared checkout
echo "==> sync dexdo @ ${DEXDO_SHA}"
[ -d "$DIR/.git" ] || git clone "$REPO" "$DIR"
git -C "$DIR" fetch --depth 1 origin "$DEXDO_SHA" && git -C "$DIR" checkout -f FETCH_HEAD
[ "$(git -C "$DIR" rev-parse HEAD)" = "$DEXDO_SHA" ] || { echo "FATAL: checkout HEAD != DEXDO_SHA"; exit 66; }

# VERIFY BY RUNNING IT, NOT BY FINDING IT. `command -v` answers "a file of that
# name is on PATH and has the exec bit" — which is also what a truncated or
# half-written download looks like, and exec'ing one of those is how a shell
# produces 126. This is hardening, not the fix for any observed failure: #300's
# 126 came from the pipeline timeout, not from here. It stays because the check
# it replaces cannot tell a healthy binary from a corrupt one, and the same
# untested pattern sits in `sdk-proof-on-host.sh`.
#
# Reinstall once and re-verify. Twice is not a retry loop: a download that lands
# broken twice is not a flake, and looping would only delay the report.
ensure_nextest() {
  cargo nextest --version >/dev/null 2>&1 && return 0
  echo "==> installing cargo-nextest"
  curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C "$HOME/.cargo/bin"
  cargo nextest --version >/dev/null 2>&1 && return 0
  echo "FATAL: cargo-nextest is present but will not run -- the download is corrupt"
  echo "--- cargo nextest --version ---"
  cargo nextest --version || true
  ls -l "$HOME/.cargo/bin/cargo-nextest" || true
  exit 69
}
ensure_nextest

cd "$DIR"
START=$(date +%s)
FILTER='binary(e2e_observer)'

# There is deliberately NO deadline here. The values live in one place — the test's
# `DEFAULT_DEADLINE_SECS` and `CAPTURE_DEADLINE_SECS` constants — and are printed from
# there together with the step's worst case (three `#[ignore]` tests under
# `--test-threads 1`, i.e. the sum of the three deadlines). A second source of the same
# quantity would drift silently: editing the Rust side would leave this script honestly
# printing an untruth. To override the deadline on the stand, add
# `OBSERVER_DEADLINE_SECS` to the ssh call's variable list in e2e.yml — the script
# forwards it while knowing nothing about the value, and the Rust side applies it to the
# two convergence tests only (the capture anchor does not wait on convergence).

# An empty selection is not a harmless no-op: `--run-ignored only` with a filter that
# matches nothing exits zero, and the step would report success having checked nothing
# — exactly the failure mode the observer exists for. stderr is kept: "the filter
# selected nothing" and "the crate did not build" are indistinguishable by line count.
LIST_ERR=$(mktemp)
N=$(cargo nextest list -p dodex-infrastructure --run-ignored only -E "$FILTER" 2>"$LIST_ERR" | grep -c '::' || true)
if [ "$N" -lt 1 ]; then
  echo "FATAL: empty nextest selection for filter $FILTER"
  echo "--- cargo nextest list stderr ---"
  cat "$LIST_ERR"
  rm -f "$LIST_ERR"
  exit 65
fi
rm -f "$LIST_ERR"

lease_assert                                   # immediately before reading the database
#
# `--profile ci-e2e` is not taken out of inertia. Its overrides
# (`.config/nextest.toml`) name `dodex-api` binaries and do not apply here; what it
# actually provides is `slow-timeout = 60s × terminate-after 10`, i.e. a hard 600 s per
# test. That is not a duplicate of the observer's deadline but a different limit: the
# deadline, whatever it is, covers NON-convergence but not a hung query —
# `acquire_timeout` bounds only establishing a connection, and nothing bounds executing
# a query. The 600 s is that insurance.
#
# The `set +e` around the run is not stylistic: the script runs under
# `set -euo pipefail`, and a failing `cargo nextest run` would terminate it right on
# that line, so neither `RC=$?` nor the budget print below would execute. The print is
# needed most on a red run, and the first run on the stand is the most likely to be
# red. The step's exit code is unchanged — it is restored by an explicit `exit`.
set +e
cargo nextest run --profile ci-e2e --color never -p dodex-infrastructure \
  --run-ignored only --test-threads 1 -E "$FILTER"
RC=$?
set -e
# The pipeline budget is described as tight. `START` is taken before `nextest list`,
# so the number includes compilation: that is where it happens, not in `run`.
echo "==> observer wall clock (compile + run): $(( $(date +%s) - START ))s, rc=$RC"
exit "$RC"
