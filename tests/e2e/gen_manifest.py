#!/usr/bin/env python3
"""Builds dex_contracts_manifest.json from a staged set of compiled contracts
(design spec §8.1-2).

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
    out = subprocess.run([cli, "-j", "decode", "stateinit", "--tvc", path],
                         capture_output=True, text=True, check=True).stdout
    j = json.loads(out)
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

    contracts = []
    for d in a.staged:
        for f in sorted(os.listdir(d)):
            if not f.endswith(".tvc"): continue
            name = f[:-4]; abi = os.path.join(d, name + ".abi.json")
            if not os.path.exists(abi): sys.exit(f"FATAL: no ABI for {name}")
            ch, cd = tvc_code_hash(a.cli, os.path.join(d, f))
            contracts.append({"name": name,
                              "code_hash": ch, "code_depth": cd,
                              "abi_sem_hash": abi_sem_hash(abi),
                              "tvc_rel_path": f if name in ("PrivateNote", "PMP", "OrderBook") else None})
    if len(contracts) != 13: sys.exit(f"FATAL: expected 13 contracts, found {len(contracts)}")

    # Fail-closed on unset pins: os.environ.get(..., "") would otherwise
    # silently record "unpinned" provenance for a run nobody can reproduce.
    required_env = ["CI_COMMIT_SHA", "ACKI_SHA", "SOLD_VER", "SOLD_OLD_VER", "TVM_VER",
                    "DEXDO_NODE_IMG", "DEXDO_GQL_IMG", "DEXDO_BM_IMG", "DEXDO_ALLBIN_IMG"]
    missing = [v for v in required_env if not os.environ.get(v)]
    if missing: sys.exit(f"FATAL: unset pins: {missing}")
    for v in ("DEXDO_NODE_IMG", "DEXDO_GQL_IMG", "DEXDO_BM_IMG", "DEXDO_ALLBIN_IMG"):
        if "@sha256:" not in os.environ[v]:
            sys.exit(f"FATAL: {v} must be image@sha256:… (design spec §8.1) -- "
                      "a tag is rewritable and is not a pin")

    # Vanity addresses of the force-placed AI roots, read out of the pinned
    # generator source itself rather than hand-entered: ModelRegistry has no
    # Rust wrapper at all, and SuperRoot's wrapper carries no DEFAULT_ADDRESS,
    # so this manifest is the only place the preflight can learn either
    # address from -- and sourcing it from the generator means the manifest
    # can never describe an address the generator does not actually use.
    zs = open(a.zerostate_py, encoding="utf-8").read()
    deployed = {}
    for key, var in {"super_root": "AI_SUPER_ROOT_ADDRESS",
                     "model_registry": "AI_MODEL_REGISTRY_ADDRESS"}.items():
        m = re.search(rf'^{var}\s*=\s*"(0:[0-9a-f]{{64}})"', zs, re.M)
        if not m: sys.exit(f"FATAL: {var} not found in {a.zerostate_py}")
        deployed[key] = m.group(1)

    manifest = {
        "dodex_sha": os.environ.get("CI_COMMIT_SHA", "local"),
        "acki_sha": os.environ.get("ACKI_SHA", "unpinned"),
        "sold_ver": os.environ.get("SOLD_VER", ""), "sold_old_ver": os.environ.get("SOLD_OLD_VER", ""),
        "tvm_cli_ver": os.environ.get("TVM_VER", ""),
        "images": {k: os.environ.get(v, "") for k, v in
                   {"node": "DEXDO_NODE_IMG", "gql": "DEXDO_GQL_IMG", "bm": "DEXDO_BM_IMG", "allbin": "DEXDO_ALLBIN_IMG"}.items()},
        "contracts": contracts,
        "deployed_addresses": deployed,
    }
    with open(a.out, "w", encoding="utf-8") as f: json.dump(manifest, f, indent=1, sort_keys=True)


if __name__ == "__main__": main()
