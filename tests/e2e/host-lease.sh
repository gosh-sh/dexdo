#!/usr/bin/env bash
# Host-wide pipeline lease for the shared e2e host (design spec §8.3).
#
# More than one CI pipeline can reach this host, but only one may hold the
# network at a time: `up.sh` tears down and recreates the whole stand as its
# first step, so a second pipeline starting while a first is still testing
# would wipe the network out from under it. This lease is the arbitration
# between pipelines; it says nothing about processes within a single run
# (that is `ledger.lock`/`b0.lock`'s job).
#
# The guard is a fixed path outside any checkout: /var/lock/dexdo-e2e.lock.
# It is never renamed, replaced, or removed, so every acquire/renew/assert/
# release/takeover always flocks the SAME inode. A lock held on a file that
# gets replaced (e.g. by a config-delivery step overwriting a checkout)
# protects nothing, because the next process just opens a different inode
# and never contends with the first lock at all.
#
# The lease data lives in a separate file, /var/lock/dexdo-e2e.lease, holding
# "pipeline_id timestamp". Keeping the mutable data out of the guard file
# means the guard's own inode never has to be touched to update the lease.
#
# Usage: host-lease.sh acquire|renew|assert|release <pipeline_id> [ttl_secs]
set -euo pipefail

GUARD=/var/lock/dexdo-e2e.lock
LEASE=/var/lock/dexdo-e2e.lease

CMD=${1:?usage: host-lease.sh acquire|renew|assert|release <pipeline_id> [ttl_secs]}
ID=${2:?pipeline_id is required}
# TTL only guards against a DEAD pipeline (crashed or killed without
# releasing). A LIVE one never hits it: every step that touches the host
# calls `assert` before doing anything, and `assert` renews the timestamp as
# it checks it (see do_assert below), and sdk-proof-on-host.sh additionally
# renews every 300s in the background for the parts of a run no discrete
# pipeline step is asserting through. 5400s (90min) covers cold sdk
# compilation on host B plus the longest single step, with margin.
TTL=${3:-5400}

# All state transitions run under this flock. Acquiring, reading the current
# lease, deciding whether it is free/ours/expired, and writing the new owner
# are one atomic unit here — never separate operations a second process could
# interleave with. A naive check-then-write (read lease, decide, THEN flock)
# would let two pipelines both observe the same expired lease and both
# conclude they're allowed to take it.
with_guard() {
  exec 9>"$GUARD"
  flock -w 30 9 || { echo "FATAL: guard busy (flock on $GUARD not acquired within 30s)"; exit 70; }
  "$@"
}

_now() { date +%s; }

_read() {
  if [ -f "$LEASE" ]; then
    read -r L_ID L_TS < "$LEASE"
  else
    L_ID=""
    L_TS=0
  fi
}

do_acquire() {
  _read
  if [ -n "$L_ID" ] && [ "$L_ID" != "$ID" ]; then
    AGE=$(( $(_now) - L_TS ))
    if [ "$AGE" -lt "$TTL" ]; then
      echo "FATAL: live lease held by pipeline $L_ID (age ${AGE}s < TTL ${TTL}s)"
      exit 71
    fi
  fi
  # Reaching here means the lease is ours already, absent, or expired.
  # Takeover of an expired lease happens right here, still under the guard's
  # flock, with the freshness re-checked above after acquiring it (not
  # before) -- so a second pipeline racing to take over the same expired
  # lease cannot also see it as free: whichever one gets the flock first
  # writes and wins, and the second sees the new owner once it gets its turn.
  echo "$ID $(_now)" > "$LEASE"
  echo "lease acquired by $ID"
}

do_renew() {
  _read
  [ "$L_ID" = "$ID" ] || { echo "FATAL: lease is not ours (held by ${L_ID:-nobody})"; exit 72; }
  echo "$ID $(_now)" > "$LEASE"
}

do_assert() {
  _read
  [ "$L_ID" = "$ID" ] || { echo "FATAL: owner check failed ($L_ID != $ID)"; exit 72; }
  # assert IS the heartbeat: every pipeline step that touches the host calls
  # this before doing anything, and this write is what keeps the lease from
  # aging past TTL while the pipeline that owns it is still alive and taking
  # steps. Without this renew, a long step would let the lease go stale out
  # from under a run that is still very much in progress.
  echo "$ID $(_now)" > "$LEASE"
}

do_release() {
  _read
  [ "$L_ID" = "$ID" ] || { echo "FATAL: refusing to release a lease this pipeline doesn't own"; exit 72; }
  rm -f "$LEASE"
}

case "$CMD" in
  acquire|renew|assert|release) with_guard "do_$CMD" ;;
  *) echo "FATAL: unknown command '$CMD' (want acquire|renew|assert|release)"; exit 64 ;;
esac
