#!/usr/bin/env python3
"""Builds dex_contracts_manifest.json from a staged set of compiled contracts.

Run against the output of tests/e2e/stage_contracts.sh: a directory per
versioned path, each holding <name>.tvc/<name>.abi.json pairs for the fixed
13-contract allowlist that script stages. The manifest this produces is the
provenance record sdk/tests/integration/common/preflight.rs checks a running
stand against before any e2e scenario touches it — a code hash and depth plus
a semantic ABI hash per contract, the toolchain/runtime pins the build was
made with, and the two force-placed AI-registry addresses the zerostate
generator bakes in at fixed vanity addresses.

`--selftest` prints sha256 of the canonical form of `{"a": 1}` and exits
nonzero if it doesn't match the same literal preflight.rs pins in its own
`abi_semantic_hash_pins_cross_language_reference` test — the one place the
two languages' notions of "canonical JSON" are checked against each other
rather than each just against itself.
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys


def canonical_json(obj):
    """The canonical form every semantic-hash comparison in this pipeline is
    built on: sorted keys, no incidental whitespace. The Rust side
    (preflight.rs's `canonical_json`) hand-rolls the same rule recursively,
    because serde_json there is insertion-ordered (a transitive feature pulled
    in by the tvm stack) rather than because Python needs the help --
    `sort_keys`+`separators` already gives the identical byte string here.
    """
    return json.dumps(obj, sort_keys=True, separators=(",", ":"))


def abi_sem_hash(path):
    with open(path, encoding="utf-8") as f:
        obj = json.load(f)
    return hashlib.sha256(canonical_json(obj).encode()).hexdigest()


def tvc_code_hash(cli, path):
    # tvm-cli decode stateinit --tvc prints JSON with code_hash/code_depth.
    # This is always the RAW (unsalted) hash, including for PMP and
    # OrderBook, which are deployed on-chain with salted code: the preflight
    # derives the salted form itself, on the host, from these exact shipped
    # bytes, so a salted value recorded here would only ever be compared
    # against another salted value and would never actually confirm the
    # shipped TVC is what got deployed. The depth travels with the hash
    # because it participates in deriving child addresses (PrivateNote.sol)
    # -- a right hash paired with a wrong depth still resolves to the wrong
    # accounts.
    try:
        proc = subprocess.run([cli, "-j", "decode", "stateinit", "--tvc", path],
                              capture_output=True, text=True, check=True)
    except subprocess.CalledProcessError as e:
        # Without this, a real tvm-cli failure surfaces as a generic
        # CalledProcessError with no hint of *why* -- and tvm-cli's stderr is
        # exactly the diagnostic a maintainer needs (bad tvc, wrong version,
        # missing arg).
        sys.exit(f"FATAL: `{cli} -j decode stateinit --tvc {path}` failed "
                 f"(exit {e.returncode}): {e.stderr.strip()}")
    except OSError as e:
        sys.exit(f"FATAL: cannot run tvm-cli at `{cli}`: {e}")
    j = json.loads(proc.stdout)
    return j["code_hash"], int(j["code_depth"])


# Cross-language pin: preflight.rs's `abi_semantic_hash_pins_cross_language_
# reference` test asserts this exact value for the same input. If the two
# ever disagree, every ABI comparison the preflight makes is meaningless --
# each side would be hashing what it thinks is the same document into two
# different digests without either one noticing.
SELFTEST_EXPECTED = "015abd7f5cc57a2dd94b7590f04ad8084273905ee33ec5cebeae62276a97f862"


def selftest():
    got = hashlib.sha256(canonical_json({"a": 1}).encode()).hexdigest()
    print(got)
    return 0 if got == SELFTEST_EXPECTED else 1


# name -> env var, for the four runtime images the manifest pins.
IMAGE_VARS = {"node": "DEXDO_NODE_IMG", "gql": "DEXDO_GQL_IMG",
             "bm": "DEXDO_BM_IMG", "allbin": "DEXDO_ALLBIN_IMG"}
REQUIRED_ENV = ["CI_COMMIT_SHA", "ACKI_SHA", "SOLD_VER", "SOLD_OLD_VER", "TVM_VER",
               *IMAGE_VARS.values()]
# A full image@sha256:<64 lower-case hex> reference, anchored at both ends.
# Anything else -- a bare tag, a digest sitting inside a longer string with
# trailing noise, embedded whitespace -- is rejected outright. "contains the
# substring @sha256:" is not the same check and does not defend against any
# of those.
IMAGE_DIGEST_RE = re.compile(r"^[^\s@]+@sha256:[0-9a-f]{64}$")


def check_env_pins():
    """Fail-closed on every build/runtime pin the manifest records, and
    fail-first: this runs before the per-contract loop below, which spawns a
    tvm-cli subprocess per contract, so a misconfigured environment is
    reported immediately rather than after paying for 13 of those.
    """
    # os.environ.get(v, "") elsewhere in this file would silently record
    # "unpinned" provenance for a run nobody can reproduce -- so every one of
    # these has to be present and non-empty, checked as a batch so a single
    # run reports everything missing at once.
    missing = [v for v in REQUIRED_ENV if not os.environ.get(v)]
    if missing:
        sys.exit(f"FATAL: unset pins: {missing}")
    for v in IMAGE_VARS.values():
        if not IMAGE_DIGEST_RE.match(os.environ[v]):
            sys.exit(f"FATAL: {v}={os.environ[v]!r} is not image@sha256:<64 lower-case hex> "
                      "-- a tag is rewritable and is not a pin")


def vanity_addresses(zerostate_py):
    """Vanity addresses of the force-placed AI roots, read out of the pinned
    generator source itself rather than hand-entered: ModelRegistry has no
    Rust wrapper at all, and SuperRoot's wrapper carries no DEFAULT_ADDRESS,
    so this manifest is the only place the preflight can learn either address
    from -- and sourcing it from the generator means the manifest can never
    describe an address the generator does not actually use.
    """
    zs = open(zerostate_py, encoding="utf-8").read()
    deployed = {}
    for key, var in {"super_root": "AI_SUPER_ROOT_ADDRESS",
                     "model_registry": "AI_MODEL_REGISTRY_ADDRESS"}.items():
        m = re.search(rf'^{var}\s*=\s*"(0:[0-9a-f]{{64}})"', zs, re.M)
        if not m:
            sys.exit(f"FATAL: {var} not found in {zerostate_py}")
        deployed[key] = m.group(1)
    return deployed


def build_contracts(staged_dirs, cli):
    contracts = []
    for d in staged_dirs:
        for f in sorted(os.listdir(d)):
            if not f.endswith(".tvc"): continue
            name = f[:-4]; abi = os.path.join(d, name + ".abi.json")
            if not os.path.exists(abi): sys.exit(f"FATAL: no ABI for {name}")
            ch, cd = tvc_code_hash(cli, os.path.join(d, f))
            contracts.append({"name": name,
                              "code_hash": ch, "code_depth": cd,
                              "abi_sem_hash": abi_sem_hash(abi),
                              "tvc_rel_path": f if name in ("PrivateNote", "PMP", "OrderBook") else None})
    if len(contracts) != 13:
        sys.exit(f"FATAL: expected 13 contracts, found {len(contracts)}")
    return contracts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--staged", nargs="+")
    ap.add_argument("--out")
    ap.add_argument("--zerostate-py")  # the pinned generator -- source of the vanity addresses
    ap.add_argument("--cli", default=os.environ.get("CLI", "tvm-cli"))
    ap.add_argument("--selftest", action="store_true",
                     help='print sha256 of the canonical form of {"a": 1} and exit; '
                          'takes no other argument, touches no filesystem or network')
    a = ap.parse_args()

    if a.selftest:
        sys.exit(selftest())
    if not (a.staged and a.out and a.zerostate_py):
        ap.error("--staged, --out and --zerostate-py are required unless --selftest is given")

    check_env_pins()
    deployed = vanity_addresses(a.zerostate_py)
    contracts = build_contracts(a.staged, a.cli)

    # Every value below is guaranteed present by check_env_pins() above, so
    # this reads straight from os.environ rather than through a
    # os.environ.get(..., fallback) that could never actually be reached --
    # a reachable-looking fallback here would read as though an unpinned
    # manifest were a possible output, which is exactly the impression this
    # design must not give.
    manifest = {
        "dodex_sha": os.environ["CI_COMMIT_SHA"],
        "acki_sha": os.environ["ACKI_SHA"],
        "sold_ver": os.environ["SOLD_VER"], "sold_old_ver": os.environ["SOLD_OLD_VER"],
        "tvm_cli_ver": os.environ["TVM_VER"],
        "images": {k: os.environ[v] for k, v in IMAGE_VARS.items()},
        "contracts": contracts,
        "deployed_addresses": deployed,
    }
    with open(a.out, "w", encoding="utf-8") as f: json.dump(manifest, f, indent=1, sort_keys=True)


if __name__ == "__main__": main()
