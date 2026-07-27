#!/usr/bin/env bash
# Runs the SDK proof (or post-proof acceptance) suite on the shared e2e host
# (host B), against the network `up.sh` already brought up there (design spec
# §8.3). Required env: DEXDO_SHA, E2E_RUN_ID, ACKI_DIR, PIPELINE_ID.
# Optional: SUITE=proof|acceptance (default proof), DEXDO_REPO, DEXDO_DIR.
#
# This script is always piped over SSH from the CI runner's own checkout
# (`... bash -s" < tests/e2e/sdk-proof-on-host.sh`), the same mechanism the
# existing api-e2e runner uses -- never executed from the shared checkout
# already sitting on host B. That checkout is exactly the shared state this
# script is about to overwrite, and it may currently hold whatever another
# pipeline last left there. For the same reason, the lease-protocol pieces
# below are inlined rather than sourced from host-lease.sh: nothing that
# guards the host may be read off host B's disk.
set -euo pipefail

: "${DEXDO_SHA:?}" "${E2E_RUN_ID:?}" "${ACKI_DIR:?}" "${PIPELINE_ID:?}"

REPO="${DEXDO_REPO:-https://github.com/gosh-sh/dexdo.git}"
DIR="${DEXDO_DIR:-$HOME/dexdo-e2e}"
# shellcheck disable=SC1091  # host-specific path; rustup puts it there on this host's own prior setup
source "$HOME/.cargo/env"

# Same guard/lease paths as host-lease.sh -- both must agree on where the
# lease lives, or they'd be arbitrating different, unrelated locks.
GUARD=/var/lock/dexdo-e2e.lock
LEASE=/var/lock/dexdo-e2e.lease

# Owner check is hard, never `|| true`: losing the host lease is exactly the
# condition this whole protocol exists to catch, and swallowing that error
# here would mean the rest of the script runs against a host somebody else
# now legitimately owns.
lease_assert() {
  exec 9>"$GUARD"
  flock -w 30 9
  read -r L_ID _ < "$LEASE" 2>/dev/null || L_ID=""
  [ "$L_ID" = "$PIPELINE_ID" ] || { echo "FATAL: lease is not ours ($L_ID != $PIPELINE_ID)"; exit 72; }
  echo "$PIPELINE_ID $(date +%s)" > "$LEASE"
  exec 9>&-
}

lease_assert                                   # before the sync below touches the shared checkout
echo "==> sync dodex @ ${DEXDO_SHA}"
[ -d "$DIR/.git" ] || git clone "$REPO" "$DIR"
git -C "$DIR" fetch --depth 1 origin "$DEXDO_SHA" && git -C "$DIR" checkout -f FETCH_HEAD
[ "$(git -C "$DIR" rev-parse HEAD)" = "$DEXDO_SHA" ] || { echo "FATAL: checkout HEAD != DEXDO_SHA"; exit 66; }
lease_assert                                   # again immediately before driving the chain

# The proof scenario's own hard timeout is 50 minutes and cold sdk
# compilation is unbounded, both comfortably longer than any single
# host-lease TTL window. Left unrenewed for that long, this run's own lease
# would go stale while it is still very much alive, and a legitimate takeover
# by another pipeline would tear the network down mid-run. Renewing here,
# every 300s, is what keeps that from happening for as long as this script
# is between the discrete steps that otherwise assert on their own.
#
# Losing the lease here is fatal to the whole script, not just this
# background loop: `kill -TERM -$$` signals the process GROUP, not just this
# script's own PID. Signaling just `$$` would only kill the orchestrating
# shell and leave `cargo nextest run` (a separate foreground child of this
# same group) running as an orphan, still driving the chain -- verified
# empirically before writing this: signaling a single PID leaves its
# foreground child alive, while signaling the group with a leading `-` tears
# down both. `$$` in this subshell still names the top-level script's PID,
# not the subshell's own -- bash does not rebind it on entering `( ... )`.
(
  while sleep 300; do
    ( lease_assert ) || {
      echo "FATAL: lost the host lease during the run -- aborting"
      kill -TERM -$$
      break
    }
  done
) &
HB=$!
trap 'kill "$HB" 2>/dev/null || true' EXIT

command -v cargo-nextest >/dev/null 2>&1 \
  || curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C "$HOME/.cargo/bin"

SEED="$ACKI_DIR/config/dex_test_notes.keys.json"
MANIFEST="$ACKI_DIR/config/dex_contracts_manifest.json"
LEDGER_DIR="$(dirname "$SEED")"

# Closes the provenance chain for host B's own acki-nacki checkout: the
# manifest records the acki-nacki SHA the contracts were staged and hashed
# against, so this is where that gets checked against what is actually
# deployed on this host. (manifest.dodex_sha == DEXDO_SHA is instead checked
# by the test's own preflight, which already reads the manifest for its
# code-hash/ABI comparisons.)
ACKI_MAN=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["acki_sha"])' "$MANIFEST")
[ "$(git -C "$ACKI_DIR" rev-parse HEAD)" = "$ACKI_MAN" ] || { echo "FATAL: host acki-nacki checkout != manifest.acki_sha"; exit 66; }

if [ "${SUITE:-proof}" = proof ]; then
  # Starts a fresh ledger generation -- exactly once per proof run, and only
  # here. Fail-closed: any error from the binary fails this step outright,
  # with no fallback path (e.g. a script writing the ledger file directly
  # would bypass its flock protocol and could corrupt a concurrent reader).
  cargo run --quiet --manifest-path "$DIR/sdk/e2e-harness/Cargo.toml" --bin ledger-bootstrap -- \
    --dir "$LEDGER_DIR" --run-id "$E2E_RUN_ID" --manifest "$MANIFEST"
  FILTER='test(=proof_money::proof_money_lifecycle_local)'
  THREADS=1
else
  FILTER='test(parallel_setup)'
  THREADS=2
fi

cd "$DIR"
# An empty selection here is not a harmless no-op: `--run-ignored only` on a
# filter that matches nothing still exits 0, so the step would report
# success having exercised no tests at all. Count the selection before
# running and fail outright if it's empty, rather than let a typo'd filter
# masquerade as a passing suite.
N=$(cargo nextest list --manifest-path sdk/Cargo.toml --run-ignored only -E "$FILTER" 2>/dev/null | grep -c '::' || true)
[ "$N" -ge 1 ] || { echo "FATAL: empty nextest selection for filter $FILTER"; exit 65; }

E2E_NETWORK_ENDPOINT='http://127.0.0.1' \
E2E_SEED_NOTES="$SEED" E2E_MANIFEST="$MANIFEST" E2E_RUN_ID="$E2E_RUN_ID" \
cargo nextest run --manifest-path sdk/Cargo.toml \
  --config-file "$DIR/.config/nextest.toml" --profile sdk-e2e \
  --run-ignored only --test-threads "$THREADS" --no-fail-fast -E "$FILTER"
lease_assert   # final confirmation; inlined for the same reason as above
