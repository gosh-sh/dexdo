"""
Generate 100 voucherGenerated events on a local network.

For each voucher:
  1. Generate random sk_u (BN254 field element)
  2. Compute sk_u_commit = poseidon_hash([sk_u, 0]) via sk-commit-tool
  3. Call RootPN.generatevoucher via multisig sendTransaction with ECC
  4. Capture voucherGenerated event BOC
  5. Save (sk_u, sk_u_commit, boc) to output file

Also verifies once that sk_u_commit produces a valid ZK proof + PrivateNote deploy.

Run:
  NETWORK=localhost python3 tests/dex/generate_vouchers.py
"""

import json
import os
import random
import subprocess
import sys
import time

sys.path.append("tests")
from helper import common

# ── Addresses ─────────────────────────────────────────────────────────────────
GIVER_ADDRESS = "0:1111111111111111111111111111111111111111111111111111111111111111"
ROOT_PN_ADDRESS = "0:1010101010101010101010101010101010101010101010101010101010101010"

# ── ABIs ──────────────────────────────────────────────────────────────────────
GIVER_ABI = "./contracts/giver/GiverV3.abi.json"
GIVER_KEY_PATH = "./tests/GiverV3.keys.json"
ROOT_PN_ABI = "./contracts/0.79.3_compiled/dex/RootPN.abi.json"
MSIG_ABI = "./contracts/0.79.3_compiled/updatecustodianmultisigwallet/UpdateCustodianMultisigWallet.abi.json"
MSIG_TVC = "./contracts/0.79.3_compiled/updatecustodianmultisigwallet/UpdateCustodianMultisigWallet"

# ── Tools ─────────────────────────────────────────────────────────────────────
SK_COMMIT_TOOL = "/home/sehor/work/sk-commit-tool/target/release/sk-commit-tool"
HALO2_PROOVER = "./halo2-proover"

# ── Keys ──────────────────────────────────────────────────────────────────────
MSIG_KEY_PATH = "./tests/dex/msig_voucher.keys.json"
EPHEMERAL_KEY_PATH = "./tests/dex/ephemeral_voucher.keys.json"

# ── Constants ─────────────────────────────────────────────────────────────────
TOKEN_TYPE_SHELL = 2
VOUCHER_NOMINAL = 100_000_000_000  # 100 shell — smallest allowed
NUM_VOUCHERS = 100
OUTPUT_FILE = "./tests/dex/vouchers.txt"


def log(title, data):
    print(f"\n=== {title} ===")
    print(data)
    print("===")


def generate_random_sk():
    """Generate a random 32-byte BN254 field element (last byte < 0x30)."""
    sk_bytes = [random.randint(0, 255) for _ in range(31)] + [random.randint(0, 0x2F)]
    return bytes(sk_bytes).hex()


def compute_sk_commit(sk_u_hex):
    """Compute sk_u_commit = poseidon_hash([sk_u, 0]) using sk-commit-tool."""
    result = subprocess.check_output(
        [SK_COMMIT_TOOL, sk_u_hex], stderr=subprocess.STDOUT
    ).decode("utf-8").strip()
    return result


def encode_generatevoucher_body(sk_u_commit_hex):
    """Encode the generatevoucher function call body via tvm-cli."""
    params_json = json.dumps({
        "sk_u_commit": f"0x{sk_u_commit_hex}",
        "isFee": False
    })
    cmd = f'{common.TVM_CLI} -j body --abi {ROOT_PN_ABI} generatevoucher \'{params_json}\''
    output = subprocess.check_output(cmd, shell=True, stderr=subprocess.STDOUT).decode("utf-8").strip()
    result = json.loads(output)
    return result["Message"]


VOUCHER_EVENT_DST = ":0000000000000000000000000000000000000000000000000000000000000087"

def get_event_count():
    """Get current count of voucherGenerated events from RootPN."""
    cmd = (
        f'{common.TVM_CLI} -j query-raw messages "id" '
        f'--filter \'{{"src":{{"eq":"{ROOT_PN_ADDRESS}"}},"msg_type":{{"eq":2}},"dst":{{"eq":"{VOUCHER_EVENT_DST}"}}}}\''
    )
    output = subprocess.check_output(cmd, shell=True, stderr=subprocess.STDOUT).decode("utf-8").strip()
    try:
        msgs = json.loads(output)
        return len(msgs)
    except json.JSONDecodeError:
        return 0


def get_latest_events(limit=1):
    """Get the latest voucherGenerated events from RootPN."""
    cmd = (
        f'{common.TVM_CLI} -j query-raw messages "id boc body src dst created_at" '
        f'--filter \'{{"src":{{"eq":"{ROOT_PN_ADDRESS}"}},"msg_type":{{"eq":2}},"dst":{{"eq":"{VOUCHER_EVENT_DST}"}}}}\' '
        f'--limit {limit} '
        f'--order \'[{{"path":"created_at","direction":"DESC"}}]\''
    )
    output = subprocess.check_output(cmd, shell=True, stderr=subprocess.STDOUT).decode("utf-8").strip()
    try:
        return json.loads(output)
    except json.JSONDecodeError:
        return []


def deploy_multisig():
    """Deploy a multisig wallet and fund it with native + ECC."""
    print("\n--- Deploying multisig wallet ---")

    # Make a copy of TVC to avoid modifying the original
    import shutil
    msig_tvc_copy = "/tmp/msig_voucher/UpdateCustodianMultisigWallet"
    os.makedirs("/tmp/msig_voucher", exist_ok=True)
    shutil.copy(f"{MSIG_TVC}.tvc", f"{msig_tvc_copy}.tvc")
    shutil.copy(MSIG_ABI, "/tmp/msig_voucher/UpdateCustodianMultisigWallet.abi.json")
    msig_abi_copy = "/tmp/msig_voucher/UpdateCustodianMultisigWallet.abi.json"

    # Let generate_address create keys (using --genkey so key matches TVC)
    # Remove existing key file to ensure fresh generation
    if os.path.exists(MSIG_KEY_PATH):
        os.remove(MSIG_KEY_PATH)
    msig_address = common.generate_address(msig_tvc_copy, MSIG_KEY_PATH)
    pubkey = common.read_public_key(MSIG_KEY_PATH)
    print(f"Multisig address: {msig_address}")
    print(f"Multisig pubkey: {pubkey}")

    # Fund from giver: native + ECC (flag 17 for uninit account)
    total_ecc_needed = NUM_VOUCHERS * VOUCHER_NOMINAL * 2  # 2x safety margin
    print(f"Funding multisig with {total_ecc_needed} ECC type {TOKEN_TYPE_SHELL}...")
    common.call_contract(
        GIVER_ADDRESS, GIVER_ABI, GIVER_KEY_PATH,
        "sendCurrencyWithFlag",
        {"dest": msig_address, "value": "100000000000000",
         "ecc": {"2": str(total_ecc_needed)}, "flag": "17"}
    )
    time.sleep(3)

    # Wait for account to appear
    common.wait_account_uninit(msig_address)

    # Deploy constructor
    constructor_params = {
        "owners_pubkey": [f"0x{pubkey}"],
        "owners_address": [],
        "reqConfirms": 1,
        "reqConfirmsData": 1,
        "value": 100_000_000
    }
    common.execute_cli_cmd(
        f"deployx --abi {msig_abi_copy} --keys {MSIG_KEY_PATH} {msig_tvc_copy}.tvc "
        f"{common.format_params(constructor_params)}",
        True
    )

    # Wait for active
    common.wait_account_active(msig_address)
    print(f"Multisig deployed at {msig_address}")

    # Verify ECC balance
    account = common.get_account(msig_address)
    log("Multisig account", account)
    ecc = account.get("ecc_balance", {})
    if not (ecc.get("2") and int(ecc["2"]) > 0):
        # ECC was lost during deploy, re-fund
        print("ECC balance lost during deploy, re-funding...")
        common.call_contract(
            GIVER_ADDRESS, GIVER_ABI, GIVER_KEY_PATH,
            "sendCurrencyWithFlag",
            {"dest": msig_address, "value": "2000000000",
             "ecc": {"2": str(total_ecc_needed)}, "flag": "1"}
        )
        time.sleep(3)
        account = common.get_account(msig_address)
        ecc = account.get("ecc_balance", {})
    assert ecc.get("2") and int(ecc["2"]) > 0, f"ECC balance is zero! {ecc}"
    print(f"ECC balance: {ecc}")

    return msig_address, msig_abi_copy


def call_generatevoucher(msig_address, msig_abi, sk_u_commit_hex):
    """Call RootPN.generatevoucher via multisig sendTransaction."""
    # Encode the function call body
    payload = encode_generatevoucher_body(sk_u_commit_hex)

    # Send via multisig
    params = {
        "dest": ROOT_PN_ADDRESS,
        "value": "2000000000",
        "cc": {"2": str(VOUCHER_NOMINAL)},
        "bounce": False,
        "flags": 1,
        "payload": payload
    }
    result = common.call_contract(
        msig_address, msig_abi, MSIG_KEY_PATH,
        "sendTransaction", params, True
    )
    log("sendTransaction result", result)


def verify_proof_and_deploy(sk_u_hex, sk_u_commit_hex):
    """Verify that sk_u_commit produces a valid ZK proof and PrivateNote can be deployed."""
    print("\n--- Verifying proof generation and PrivateNote deployment ---")

    # Generate proof using halo2-proover
    # halo2-proover expects: sk_u token_type value
    cmd = f"{HALO2_PROOVER} {sk_u_hex} {TOKEN_TYPE_SHELL} {VOUCHER_NOMINAL}"
    print(f"Running: {cmd}")
    output = common.execute_cmd(cmd, None, True, False)
    log("halo2-proover output", output)

    # Parse output
    raw = [line for line in output.splitlines() if line.strip()][-1]
    pair = json.loads(json.loads(raw))
    proof = pair["proof"]
    deposit_identifier_hash = "0x" + pair["private_note_digest"]
    value = int(pair["private_note_sum"])
    token_type = int(pair["token_type"])

    print(f"Proof generated successfully!")
    print(f"  deposit_identifier_hash: {deposit_identifier_hash}")
    print(f"  value: {value}")
    print(f"  token_type: {token_type}")

    # Generate ephemeral keypair
    common.gen_keys(EPHEMERAL_KEY_PATH)
    ephemeral_pubkey = "0x" + common.read_public_key(EPHEMERAL_KEY_PATH)

    # First send ECC tokens to RootPN (for the PrivateNote deployment)
    common.call_contract(
        GIVER_ADDRESS, GIVER_ABI, GIVER_KEY_PATH,
        "sendCurrencyWithFlag",
        {"dest": ROOT_PN_ADDRESS, "value": 2_000_000_000, "ecc": {TOKEN_TYPE_SHELL: VOUCHER_NOMINAL}, "flag": 1}
    )
    time.sleep(3)

    # Deploy PrivateNote
    deploy_params = {
        "zkproof": proof,
        "deposit_identifier_hash": deposit_identifier_hash,
        "ephemeral_pubkey": ephemeral_pubkey,
        "value": value,
        "token_type": token_type,
    }
    common.call_contract(
        ROOT_PN_ADDRESS, ROOT_PN_ABI,
        EPHEMERAL_KEY_PATH, "deployPrivateNote", deploy_params
    )
    time.sleep(3)

    # Get PrivateNote address
    addr_result = common.run_getter(
        ROOT_PN_ADDRESS, ROOT_PN_ABI,
        "getPrivateNoteAddress",
        {"deposit_identifier_hash": deposit_identifier_hash}
    )
    pn_address = addr_result.get("privateNoteAddress")
    print(f"PrivateNote address: {pn_address}")

    if pn_address and common.is_account_active(pn_address):
        print("PrivateNote deployed and active!")
        return True
    else:
        print("WARNING: PrivateNote deployment may have failed, checking...")
        time.sleep(5)
        if pn_address and common.is_account_active(pn_address):
            print("PrivateNote is now active!")
            return True
        print("PrivateNote is NOT active.")
        return False


def main():
    common.setup()

    # Verify RootPN is active
    assert common.is_account_active(ROOT_PN_ADDRESS), \
        f"RootPN is not active at {ROOT_PN_ADDRESS}"
    print("RootPN is active")

    # Deploy multisig
    msig_address, msig_abi = deploy_multisig()

    # Get initial event count
    initial_event_count = get_event_count()
    print(f"Initial event count from RootPN: {initial_event_count}")

    # Generate first sk_u for proof verification
    first_sk_u = generate_random_sk()
    first_sk_u_commit = compute_sk_commit(first_sk_u)
    print(f"\nFirst sk_u: {first_sk_u}")
    print(f"First sk_u_commit: {first_sk_u_commit}")

    # Verify proof generation and PrivateNote deployment (once)
    proof_ok = verify_proof_and_deploy(first_sk_u, first_sk_u_commit)
    assert proof_ok, "Proof verification failed! sk_u_commit may be invalid."
    print("\nProof verification PASSED!")

    # Now generate 100 vouchers
    print(f"\n{'='*60}")
    print(f"Generating {NUM_VOUCHERS} vouchers...")
    print(f"{'='*60}")

    with open(OUTPUT_FILE, "w") as f:
        for i in range(NUM_VOUCHERS):
            print(f"\n--- Voucher {i+1}/{NUM_VOUCHERS} ---")

            # Generate new sk_u (reuse first one for voucher #1)
            if i == 0:
                sk_u = first_sk_u
                sk_u_commit = first_sk_u_commit
            else:
                sk_u = generate_random_sk()
                sk_u_commit = compute_sk_commit(sk_u)

            print(f"  sk_u: {sk_u}")
            print(f"  sk_u_commit: {sk_u_commit}")

            # Get event count before call
            events_before = get_event_count()

            # Call generatevoucher
            call_generatevoucher(msig_address, msig_abi, sk_u_commit)

            # Wait for event to appear
            max_wait = 30
            event_boc = None
            for attempt in range(max_wait):
                time.sleep(1)
                events_now = get_event_count()
                if events_now > events_before:
                    # Get the latest event
                    latest = get_latest_events(1)
                    if latest:
                        event_boc = latest[0].get("boc", "")
                        break
                if attempt % 5 == 4:
                    print(f"  Waiting for event... ({attempt+1}s)")

            if event_boc:
                print(f"  Event captured! BOC length: {len(event_boc)}")
            else:
                print(f"  WARNING: No event captured after {max_wait}s")
                event_boc = "NO_EVENT"

            # Write to file: sk_u, sk_u_commit, boc, blank line
            f.write(f"{sk_u}\n")
            f.write(f"{sk_u_commit}\n")
            f.write(f"{event_boc}\n")
            f.write("\n")
            f.flush()

            print(f"  Voucher {i+1} saved")

    print(f"\n{'='*60}")
    print(f"All {NUM_VOUCHERS} vouchers generated!")
    print(f"Output saved to {OUTPUT_FILE}")
    print(f"{'='*60}")


if __name__ == "__main__":
    main()
