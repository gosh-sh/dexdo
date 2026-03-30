#!/usr/bin/env python3
"""
Update RootPN and RootOracle on shellnet.
Uses local GiverV3 (tvm-debugger) to build state cells, tvm-cli to send updateCode.

Usage:
    NETWORK=https://shellnet.ackinacki.org python3 tests/dex/update_roots.py \
        --keys /home/sehor/Downloads/config/config/PMPRoot.keys.json
"""
import argparse
import json
import os
import subprocess
import sys

NETWORK = os.getenv("NETWORK", "https://shellnet.ackinacki.org")

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
COMPILED = os.path.join(REPO, "contracts/0.79.3_compiled/dex")

ROOTPN_TVC      = os.path.join(COMPILED, "RootPN.tvc")
ROOTPN_ABI      = os.path.join(COMPILED, "RootPN.abi.json")
ROOTORACLE_TVC  = os.path.join(COMPILED, "RootOracle.tvc")
ROOTORACLE_ABI  = os.path.join(COMPILED, "RootOracle.abi.json")

PMP_TVC               = os.path.join(COMPILED, "PMP.tvc")
PRIVATENOTE_TVC       = os.path.join(COMPILED, "PrivateNote.tvc")
NULLIFIER_TVC         = os.path.join(COMPILED, "Nullifier.tvc")
ORACLE_TVC            = os.path.join(COMPILED, "Oracle.tvc")
ORACLE_EVENT_LIST_TVC = os.path.join(COMPILED, "OracleEventList.tvc")
ORDER_BOOK_TVC        = os.path.join(COMPILED, "OrderBook.tvc")

GIVER_TVC = os.path.join(REPO, "contracts/giver/GiverV3.tvc")
GIVER_ABI = os.path.join(REPO, "contracts/giver/GiverV3.abi.json")
GIVER_ADDRESS = "0:1111111111111111111111111111111111111111111111111111111111111111"

PMP_ROOT_ADDRESS    = "0:1010101010101010101010101010101010101010101010101010101010101010"
ROOT_ORACLE_ADDRESS = "0:1515151515151515151515151515151515151515151515151515151515151515"

TVM_CLI      = os.getenv("CLI_NAME", "tvm-cli")
TVM_DEBUGGER = os.getenv("DEBUGGER_NAME", "tvm-debugger")


def get_code(tvc_path):
    cmd = [TVM_CLI, "-j", "decode", "stateinit", "--tvc", tvc_path]
    r = subprocess.run(cmd, capture_output=True, text=True)
    return json.loads(r.stdout)["code"]


def read_public_key(path):
    with open(path) as f:
        return json.loads(f.read())["public"]


def run_giver(method, params):
    """Run giver method locally via tvm-debugger, return value0 cell."""
    params_str = json.dumps(params)
    cmd = [TVM_DEBUGGER, "run",
           "--address", GIVER_ADDRESS,
           "-a", GIVER_ABI,
           "-m", method,
           "-p", params_str,
           "--decode-out-messages",
           "-i", GIVER_TVC]
    r = subprocess.run(cmd, capture_output=True, text=True)
    output = r.stdout + r.stderr
    if "TVM terminated with exit code 0" not in output:
        raise RuntimeError(f"tvm-debugger failed:\n{output}")

    for line in output.splitlines():
        line = line.strip()
        try:
            obj = json.loads(line)
            if "value0" in obj:
                return obj["value0"]
        except (json.JSONDecodeError, TypeError):
            pass

    raise RuntimeError(f"Could not find value0 in output:\n{output}")


def get_version(addr, abi):
    cmd = [TVM_CLI, "-j", "--url", NETWORK, "runx",
           "--abi", abi, "--addr", addr, "-m", "getVersion"]
    r = subprocess.run(cmd, capture_output=True, text=True)
    try:
        result = json.loads(r.stdout)
        return result.get("value0", "?")
    except Exception:
        return "?"


def update_contract(name, addr, abi, tvc, cell, keys_file):
    version_before = get_version(addr, abi)
    print(f"\n{name} ({addr})")
    print(f"  version before: {version_before}")

    new_code = get_code(tvc)
    params = json.dumps({"newcode": new_code, "cell": cell})

    cmd = [TVM_CLI, "--url", NETWORK,
           "call", addr,
           "updateCode", params,
           "--abi", abi,
           "--sign", keys_file]
    r = subprocess.run(cmd, capture_output=True, text=True)
    output = r.stdout + r.stderr
    if r.returncode != 0:
        print(f"  ERROR: {output.strip()}")
        return False

    version_after = get_version(addr, abi)
    print(f"  version after:  {version_after}")
    return True


def main():
    parser = argparse.ArgumentParser(description="Update RootPN and RootOracle on shellnet")
    parser.add_argument("--keys", required=True, help="Path to PMPRoot.keys.json")
    args = parser.parse_args()

    pubkey = read_public_key(args.keys)
    print(f"Owner pubkey: {pubkey}")
    print(f"Network: {NETWORK}")

    # Build state cell for RootPN (PMP_ROOT)
    pmp_code = get_code(PMP_TVC)
    pmp_wallet_code = get_code(PRIVATENOTE_TVC)
    nullifier_code = get_code(NULLIFIER_TVC)
    oracle_code = get_code(ORACLE_TVC)
    oracle_event_list_code = get_code(ORACLE_EVENT_LIST_TVC)
    order_book_code = get_code(ORDER_BOOK_TVC)

    pmp_cell = run_giver("getDataForPMP", {
        "PMPCode": pmp_code,
        "PMPWalletCode": pmp_wallet_code,
        "NullifierCode": nullifier_code,
        "OracleCode": oracle_code,
        "OracleEventListCode": oracle_event_list_code,
        "OrderBookCode": order_book_code,
        "pubkey": f"0x{pubkey}",
    })

    # Build state cell for RootOracle
    oracle_cell = run_giver("getDataForOracle", {
        "PMPCode": pmp_code,
        "PMPWalletCode": pmp_wallet_code,
        "OracleCode": oracle_code,
        "OracleEventListCode": oracle_event_list_code,
        "pubkey": f"0x{pubkey}",
    })

    # Update RootPN
    ok1 = update_contract("RootPN", PMP_ROOT_ADDRESS, ROOTPN_ABI, ROOTPN_TVC,
                          pmp_cell, args.keys)

    # Update RootOracle
    ok2 = update_contract("RootOracle", ROOT_ORACLE_ADDRESS, ROOTORACLE_ABI, ROOTORACLE_TVC,
                          oracle_cell, args.keys)

    if ok1 and ok2:
        print("\nBoth roots updated successfully.")
    else:
        print("\nSome updates failed.")
        sys.exit(1)


if __name__ == "__main__":
    main()
