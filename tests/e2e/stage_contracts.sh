#!/usr/bin/env bash
# Stages the DEX/airegistry contracts of the current dodex checkout into the
# zerostate generator's versioned paths, then writes the provenance manifest
# from exactly those staged artifacts.
#
# Why this exists: the generator reads compiled contracts from
# contracts/{0.80.0,0.81.0}_compiled/, which acki-nacki commits and which can
# hold stale bytecode from an earlier build. This script replaces those
# directories wholesale, from a fixed allowlist of contract names, so the
# manifest gen_manifest.py produces afterward actually describes what the
# generator is about to bake into the zerostate — not last build's leftovers.
# Every step is fail-fast: a stand a later step silently tolerated would be
# indistinguishable from a correctly staged one until something on-chain
# quietly used the wrong bytecode.
set -euo pipefail
: "${BUILD_DIR:?}" "${DODEX_DIR:?}"

# Fixed by name, not derived from a directory listing: a listing would also
# pick up whatever the previous build happened to leave behind. This is the
# generator's own contract set — dex: the 8 PrivateNote-adjacent contracts
# placed straight from source; airegistry: the 5 AI-inference contracts.
#
# The compiler is chosen per CONTRACT, not per directory: RootPN and
# exchange/USDCBridge are built with 0.80.0, everything else here (plus
# exchange/DepositVoucher) with 0.81.0. Both exchange contracts are outside
# this list on purpose — this script never rebuilds them, so their committed
# artifacts survive untouched and the version split cannot be got wrong.
# Anyone extending the lists below to cover exchange/ must build USDCBridge
# with $SOLD_OLD: the zerostate generator loads its code unconditionally, so
# a wrong-compiler build lands in every zerostate with nothing to flag it.
DEX13=(PrivateNote RootPN PMP OrderBook Nullifier RootOracle Oracle OracleEventList)
AIR5=(InferenceOrderBook TokenContract RootModel SuperRoot ModelRegistry)

echo "==> [1/5] contracts from the dodex checkout"
rm -rf "$BUILD_DIR/contracts/dex" "$BUILD_DIR/contracts/airegistry"
cp -a "$DODEX_DIR/contracts/dex" "$DODEX_DIR/contracts/airegistry" "$BUILD_DIR/contracts/"

echo "==> [2/5] pin cascade (7 pin-carrying contracts; sold_old compiles RootPN)"
rm -rf /tmp/cascade_out
( cd "$BUILD_DIR" && \
  REPO="$BUILD_DIR" \
  SOLD="$BUILD_DIR/contracts/compiler/sold" \
  SOLD_OLD="$BUILD_DIR/contracts/compiler/sold_old" \
  CLI="$BUILD_DIR/contracts/compiler/tvm-cli" \
  python3 contracts/scripts/cascade_pins.py )

echo "==> [3/5] plain sold for the 6 pin-free DEX contracts"
STAGE=$(mktemp -d)
for c in PMP OrderBook Nullifier RootOracle Oracle OracleEventList; do
  # Same deterministic flags the cascade's own build() step uses: a different
  # --tvm-version/--base-path would compile different bytecode, or break the
  # imports these contracts share with the cascade-built ones.
  ( cd "$BUILD_DIR/contracts/dex" && "$BUILD_DIR/contracts/compiler/sold" \
      --tvm-version gosh --base-path .. "$c.sol" -o "$STAGE" )
done
# The cascade compiles straight into /tmp/cascade_out/{name}.tvc, with the
# matching .abi.json alongside it (sold writes both to the same -o
# directory). Copy these exact pairs by name rather than a find over the
# versioned paths below: a broad find would just as happily match an old,
# committed TVC still sitting in those paths and keep it instead of the one
# just built.
for c in PrivateNote TokenContract RootModel SuperRoot InferenceOrderBook ModelRegistry RootPN; do
  cp "/tmp/cascade_out/$c.tvc"      "$STAGE/" || { echo "FATAL: /tmp/cascade_out/$c.tvc is missing"; exit 1; }
  cp "/tmp/cascade_out/$c.abi.json" "$STAGE/" || { echo "FATAL: /tmp/cascade_out/$c.abi.json is missing — check whether sold still writes the ABI into -o, otherwise take it from the source directory instead"; exit 1; }
done
# Allowlist by name, not by count: all 13 pairs must exist in $STAGE. A count
# alone would pass even if the wrong contract were missing and a stray one
# present instead.
for c in "${DEX13[@]}" "${AIR5[@]}"; do
  if ! { test -f "$STAGE/$c.tvc" && test -f "$STAGE/$c.abi.json"; }; then
    echo "FATAL: allowlist: no $c.tvc/.abi.json pair in $STAGE"
    exit 1
  fi
done

echo "==> [4/5] clean replacement of the versioned paths + allowlist"
D080="$BUILD_DIR/contracts/0.80.0_compiled/dex"; D081="$BUILD_DIR/contracts/0.81.0_compiled"
rm -rf "$D080" "$D081/dex" "$D081/airegistry"; mkdir -p "$D080" "$D081/dex" "$D081/airegistry"
cp "$STAGE/RootPN.tvc" "$STAGE/RootPN.abi.json" "$D080/"
for c in "${DEX13[@]}"; do [ "$c" = RootPN ] && continue; cp "$STAGE/$c.tvc" "$STAGE/$c.abi.json" "$D081/dex/"; done
for c in "${AIR5[@]}";  do cp "$STAGE/$c.tvc" "$STAGE/$c.abi.json" "$D081/airegistry/"; done
# "No more" check: exactly 13 tvc files across the three directories this
# script actually replaced above. Scoped to those three, not the whole
# 0.81.0_compiled tree -- that tree has other subdirectories this script
# never touches (e.g. exchange/), and counting over it would fail a
# perfectly staged run over an artifact this script has no business judging.
n_tvc=$(find "$D080" "$D081/dex" "$D081/airegistry" -name '*.tvc' | wc -l)
[ "$n_tvc" -eq 13 ] || { echo "FATAL: staged paths hold $n_tvc tvc files, expected 13"; exit 1; }
# The one artifact outside those three directories that the generator still
# loads unconditionally, and the only other 0.80.0 contract. This script has
# no business rebuilding it -- but `rm -rf` on a sibling path is one typo away
# from taking it out, and its absence would surface as a generator failure
# well downstream of the cause.
test -f "$BUILD_DIR/contracts/0.80.0_compiled/exchange/USDCBridge.tvc" \
  || { echo "FATAL: exchange/USDCBridge.tvc is gone from the 0.80.0 tree — this script must not touch exchange/"; exit 1; }

echo "==> [5/5] manifest from the staged artifacts"
# Same tvm-cli binary the pin cascade above was pinned to (not whatever
# "tvm-cli" resolves to on PATH) -- the manifest's code hashes must come from
# the exact tool version the rest of this pipeline pins.
python3 "$DODEX_DIR/tests/e2e/gen_manifest.py" --staged "$D080" "$D081/dex" "$D081/airegistry" \
  --zerostate-py "$BUILD_DIR/contracts/scripts/generate_zerostate.py" \
  --cli "$BUILD_DIR/contracts/compiler/tvm-cli" \
  --out "$BUILD_DIR/config/dex_contracts_manifest.json"
# Ship the exact TVC bytes alongside the manifest so host B can derive the
# salted PMP/OrderBook hash from them instead of trusting a value nobody
# there can recompute.
cp "$STAGE/PrivateNote.tvc" "$STAGE/PMP.tvc" "$STAGE/OrderBook.tvc" "$BUILD_DIR/config/"
