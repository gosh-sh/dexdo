#!/usr/bin/env bash
# Runs the SDK proof (or post-proof acceptance) suite on the shared e2e host
# (host B), against the network `up.sh` already brought up there.
# Required env: DEXDO_SHA, E2E_RUN_ID, ACKI_DIR, PIPELINE_ID.
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

# A whitespace-bearing id would corrupt the "id timestamp" lease line on
# write (read splits on the whitespace on the way back in), locking this
# pipeline out of renewing, asserting, or releasing its own lease until it
# ages out on its own. Reject before it's ever written.
case "$PIPELINE_ID" in
  *[[:space:]]*) echo "FATAL: PIPELINE_ID must not contain whitespace: '$PIPELINE_ID'"; exit 64 ;;
esac

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
#
# Both the flock wait and the ownership check below fail via an explicit
# `exit`, never by letting a bare command's own non-zero status fall through
# to `errexit`. This matters beyond style: this function also runs as
# `( lease_assert ) || { ... }` inside the heartbeat further down, and bash
# suspends `errexit` for the entire body of a command used as the left side
# of `||` -- a bare `flock -w 30 9` timing out in that position would NOT
# abort the subshell, so execution would fall through into the read/compare/
# write below without ever having held the guard. An explicit `exit` inside
# the function always terminates it regardless of that suspension, which is
# why every failure path here calls `exit` directly instead of relying on
# the ambient `set -e`.
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

lease_assert                                   # before the sync below touches the shared checkout
echo "==> sync dodex @ ${DEXDO_SHA}"
[ -d "$DIR/.git" ] || git clone "$REPO" "$DIR"
git -C "$DIR" fetch --depth 1 origin "$DEXDO_SHA" && git -C "$DIR" checkout -f FETCH_HEAD
[ "$(git -C "$DIR" rev-parse HEAD)" = "$DEXDO_SHA" ] || { echo "FATAL: checkout HEAD != DEXDO_SHA"; exit 66; }
lease_assert                                   # again immediately before driving the chain

# Node and block-manager logs for a failed run, into this step's own output.
#
# The pipeline's dump_logs step now covers the steps that pipe this script --
# they are blocking, so their failure reaches the pipeline status and the
# `when: status: [failure]` guard fires. This dump is kept anyway, for two
# reasons that do not depend on that: it lands in the output of the step that
# actually failed, which is where a reader looks first, and it needs no CI
# status semantics to work at all. That second property is why it was written
# in the first place -- while these steps were `failure: ignore`, Woodpecker
# swallowed their error before it reached the pipeline status, so a run in
# which only they failed ended green and every failure-guarded step was
# skipped. Restoring that flag would silently take dump_logs away again.
#
# Best-effort throughout, and inside a subshell: this runs while the script is
# already on its way out, `errexit` is still armed, and a stopped container or a
# log file the stand never created must not mask the real failure or replace its
# exit status.
dump_stand_logs() {
  (
    set +e
    echo "===== stand logs after a failed run (ACKI_DIR=$ACKI_DIR) ====="
    cd "$ACKI_DIR" || exit 0
    echo '===== docker compose ps -a (state + exit codes) ====='
    docker compose -f docker/dexdo-e2e.compose.yaml ps -a 2>/dev/null
    for n in node0 node1 node2 node3 node4; do
      echo "===== $n (logs/$n/node.log, tail 120) ====="
      tail -n 120 "$ACKI_DIR/logs/$n/node.log" 2>/dev/null
      echo "----- docker logs $n (tail 60) -----"
      docker compose -f docker/dexdo-e2e.compose.yaml logs --tail=60 "$n" 2>/dev/null
    done
    echo '===== block_manager (logs/bm0/bm.log, tail 200) ====='
    tail -n 200 "$ACKI_DIR/logs/bm0/bm.log" 2>/dev/null
    echo '----- docker logs block_manager (tail 100) -----'
    docker compose -f docker/dexdo-e2e.compose.yaml logs --tail=100 block_manager 2>/dev/null
  )
  return 0
}

# `rc` is captured first: everything after it clobbers `$?`. The handler then
# re-exits with it, so the dump can never change what this script reports.
#
# `set +e` is load-bearing, not tidiness. The dump's `set +e` lives inside its
# subshell, which keeps the dump itself complete but says nothing about the
# status that subshell returns to here; with `errexit` still armed, a dump
# command that fails -- `docker compose` against a stand that is already gone,
# exactly the case worth dumping -- aborts this handler before `exit "$rc"` is
# ever reached, and the script then reports the dump's status instead of the
# suite's. Disabling `errexit` for the handler costs nothing: it is already on
# its way out, and every command below is either guarded or advisory.
on_exit() {
  rc=$?
  set +e
  # Kill the heartbeat's `sleep` BEFORE the heartbeat itself, and not only for
  # tidiness: the subshell inherits this script's stdout/stderr, which over SSH
  # are the pipes the client waits on for EOF. `sleep` is a separate process
  # that survives its parent, keeps those pipes open, and pins every step to
  # the next multiple of the heartbeat interval no matter how long the suite
  # actually took -- which is why steps measured 303s for a 14s test and 604s
  # for a 150s one. Killing the child first also ends the loop on its own: the
  # `while sleep 300` condition fails, so the subshell exits without spawning
  # another.
  pkill -P "$HB" 2>/dev/null
  kill "$HB" 2>/dev/null || true
  rm -f "$SUITE_PID_FILE"
  [ "$rc" -eq 0 ] || dump_stand_logs
  exit "$rc"
}

# The proof scenario's own hard timeout is 50 minutes and cold sdk
# compilation is unbounded, both comfortably longer than any single
# host-lease TTL window. Left unrenewed for that long, this run's own lease
# would go stale while it is still very much alive, and a legitimate takeover
# by another pipeline would tear the network down mid-run. Renewing here,
# every 300s, is what keeps that from happening for as long as this script
# is between the discrete steps that otherwise assert on their own.
#
# Losing the lease has to actually stop the chain-touching process, not just
# the shell orchestrating it. `$SUITE_PID_FILE` carries the PID of the
# backgrounded `cargo nextest run` job once it exists (see below); the
# heartbeat signals THAT job's process GROUP, not a bare PID and not this
# script's own PID -- reaching only the wrapper shell would leave nextest's
# test binaries running as orphans, still driving the chain. Signaling the
# group needs the job to actually own one: it is started under `set -m`
# (job control) specifically so backgrounding it creates a fresh process
# group, independent of whatever process group this script itself happens
# to be in -- an earlier version of this signaled `-$$` (this script's own
# assumed group), which only works when this script is invoked in a shape
# that makes it a process-group leader itself, true for some SSH command
# forms and false for others (e.g. a wrapping login shell, or any `cmd1 &&
# bash -s`). Falling back to a bare PID kill if the group signal fails, and
# unconditionally also signaling this script's own PID, are the last-resort
# nets: even if nextest somehow is not reachable by either kill, the
# orchestrating script must still not report success.
SUITE_PID_FILE=$(mktemp)
(
  while sleep 300; do
    ( lease_assert ) || {
      echo "FATAL: lost the host lease during the run -- aborting"
      if [ -s "$SUITE_PID_FILE" ]; then
        SPID=$(cat "$SUITE_PID_FILE" 2>/dev/null || true)
        if [ -n "$SPID" ]; then
          kill -TERM -"$SPID" 2>/dev/null || kill -TERM "$SPID" 2>/dev/null || true
        fi
      fi
      kill -TERM "$$" 2>/dev/null || true
      break
    }
  done
) &
HB=$!
trap on_exit EXIT

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
elif [ "$SUITE" = orders ]; then
  # Also joins the proof run's generation. Runs last of the four: it spends a
  # third deployer note and both trader notes, so anything after it would find
  # the pool empty.
  FILTER='test(=resting_orders::non_crossing_orders_rest_and_cancel_local)'
  THREADS=1
elif [ "$SUITE" = market ]; then
  # Last of the five. Like the orders suite it spends a deployer and two
  # trader notes, and the spec sizes the pool for both.
  FILTER='test(=market_orders::orders_that_must_not_rest_never_rest_local)'
  THREADS=1
elif [ "$SUITE" = ladder ]; then
  # Last of the six. Spends a deployer and two trader notes like the other
  # market-deploying suites; the spec sizes the pool for all of them.
  FILTER='test(=matching_ladder::a_taker_walks_levels_best_first_and_a_level_in_arrival_order_local)'
  THREADS=1
elif [ "$SUITE" = shutdown ]; then
  # Last of the seven. Also the slowest: it has to idle until the market's
  # `resultStart` before there is anything to observe.
  FILTER='test(=shutdown_orders::a_drain_refunds_resting_orders_and_hands_over_protocol_fees_local)'
  THREADS=1
elif [ "$SUITE" = abovepar ]; then
  # Adversarial pricing, kept in its own suite: if a price above par breaks
  # the book, the damage should not be tangled up in another scenario's
  # assertions.
  FILTER='test(=price_above_par::a_trade_above_par_costs_more_than_the_tokens_it_buys_local)'
  THREADS=1
elif [ "$SUITE" = bounce ]; then
  # The cheapest suite here: no market, no deployer, one note. The
  # counterparties it addresses are chosen because nothing is there.
  FILTER='test(=bounce_recovery::a_bounced_operation_gives_the_money_back_and_unlocks_the_note_local)'
  THREADS=1
elif [ "$SUITE" = bouncedeploy ]; then
  # The two bounce branches that move real money: a market whose oracle list
  # is not there, and orders sent before the book exists. Needs a market for
  # the second half only, and never freezes it.
  FILTER='test(=bounce_deploy::a_bounced_deploy_and_a_bounced_order_both_give_the_money_back_local)'
  THREADS=1
elif [ "$SUITE" = cancelled ]; then
  # The other way a market can end. Needs the staking window, so it brings the
  # market up in two halves and stakes in between — the only suite here that
  # does, which is why it gets the longest market of the set.
  FILTER='test(=cancelled_event::a_cancelled_event_refunds_every_stake_and_closes_the_market_local)'
  THREADS=1
elif [ "$SUITE" = segments ]; then
  # Nothing here fills: every phase is about two orders that could trade and
  # do not, or a batch the note declines. Needs both outcomes of one market,
  # which is why it splits rather than stakes.
  FILTER='test(=book_segments::segments_keep_crossing_orders_apart_and_a_batch_is_all_or_nothing_local)'
  THREADS=1
elif [ "$SUITE" = mm ]; then
  # The longest-running scenario after the proof: it quotes, gets taken,
  # unwinds and then has to wait out the market before it can settle.
  FILTER='test(=mm_cycle::a_maker_quotes_gets_taken_unwinds_and_settles_local)'
  THREADS=1
elif [ "$SUITE" = coupon ]; then
  # Three markets end to end, because a coupon needs a note with nothing and
  # the only way to arrive at one is to lose a market first. The slowest
  # scenario on the stand by some way.
  FILTER='test(=coupon_debt::a_coupon_is_won_with_and_the_debt_it_leaves_is_bet_against_local)'
  THREADS=1
elif [ "$SUITE" = forfeit ]; then
  # The only market in the suite that resolves anywhere but outcome 0, and
  # the only one that ends on a forfeit rather than a last claim.
  FILTER='test(=forfeit_close::a_market_resolves_away_from_zero_and_closes_on_a_forfeit_local)'
  THREADS=1
elif [ "$SUITE" = quorum ]; then
  # The only market in the suite with more than one oracle, and so the only
  # place a vote that does not execute can be observed at all.
  FILTER='test(=oracle_quorum::a_market_with_three_oracles_moves_only_on_two_of_them_local)'
  THREADS=1
elif [ "$SUITE" = release ]; then
  # No bootstrap: this joins the generation the proof run opened, which is
  # what lets it lease notes that scenario already returned. It takes no
  # preflight either, so it does not care that the stand has served a wave.
  FILTER='test(=usdc_release::usdc_release_local)'
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
# masquerade as a passing suite. stderr is captured rather than discarded: a
# filter that matches nothing and a `cargo nextest list` that never got that
# far because the crate failed to compile look identical from the line count
# alone, and the failure message below has to be able to tell them apart.
LIST_ERR=$(mktemp)
N=$(cargo nextest list --manifest-path sdk/Cargo.toml --run-ignored only -E "$FILTER" 2>"$LIST_ERR" | grep -c '::' || true)
if [ "$N" -lt 1 ]; then
  echo "FATAL: empty nextest selection for filter $FILTER"
  echo "--- cargo nextest list stderr ---"
  cat "$LIST_ERR"
  rm -f "$LIST_ERR"
  exit 65
fi
rm -f "$LIST_ERR"

# Backgrounded under `set -m` so this job gets its own process group (see the
# heartbeat comment above) instead of inheriting whatever group this script
# happens to run in. `wait` turns a signal-killed suite into a non-zero exit
# here, which `errexit` then turns into a failed step -- losing the lease
# mid-run has to fail the script, not just end the background loop that
# noticed it.
set -m
# No --config-file: sdk/ is its own workspace, so nextest auto-discovers
# sdk/.config/nextest.toml (which defines the sdk-e2e profile) on its own.
# Pointing --config-file at the root workspace's .config/nextest.toml instead
# is a hard error -- that file's [[profile.default.overrides]] name binaries
# that only exist in the root workspace.
E2E_NETWORK_ENDPOINT='http://127.0.0.1' \
E2E_SEED_NOTES="$SEED" E2E_MANIFEST="$MANIFEST" E2E_RUN_ID="$E2E_RUN_ID" \
cargo nextest run --manifest-path sdk/Cargo.toml --profile sdk-e2e \
  --run-ignored only --test-threads "$THREADS" --no-fail-fast -E "$FILTER" &
SUITE_PID=$!
set +m
echo "$SUITE_PID" > "$SUITE_PID_FILE"
wait "$SUITE_PID"
lease_assert   # final confirmation; inlined for the same reason as above
