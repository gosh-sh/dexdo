#!/usr/bin/env python3
"""
Shellnet DEX full integration test.

Mirrors all phases of main_test.py (except withdrawTokens and oracle withdrawFees).
Giver is used only to replenish RootPN ECC_SHELL before sendEccShellToPrivateNote.
RootOracle/RootPN pay for child deployments from their own vmshell balance.

Usage:
    NETWORK=shellnet.ackinacki.org python3 tests/dex/shellnet_test.py

Environment variables:
    NETWORK  — network endpoint (default: shellnet.ackinacki.org)
"""
import json
import os
import random
import sys
import time

sys.path.append("tests")
from helper import common


def generate_random_sk():
    """Generate a random valid sk for halo2-proover.

    The binary reads the 64-char hex as a little-endian BN254 field element.
    The last byte becomes the MSB and must be < 0x30 (field modulus starts
    with 0x30644e72…).  We keep it in [0x00..0x2f] for safety.
    """
    return os.urandom(31).hex() + format(random.randint(0, 0x2f), '02x')

# ── Contract addresses (same on all networks) ────────────────────────────────
ROOT_PN_ADDRESS = "0:1010101010101010101010101010101010101010101010101010101010101010"
ROOT_ORACLE_ADDRESS = "0:1515151515151515151515151515151515151515151515151515151515151515"
GIVER_ADDRESS = "0:1111111111111111111111111111111111111111111111111111111111111111"

# ── ABI paths ─────────────────────────────────────────────────────────────────
GIVER_ABI = "./contracts/giver/GiverV3.abi.json"
GIVER_KEY_PATH = "./tests/GiverV3.keys.json"
ROOT_PN_ABI = "./contracts/0.79.3_compiled/dex/RootPN.abi.json"
ROOT_ORACLE_ABI = "./contracts/0.79.3_compiled/dex/RootOracle.abi.json"
ORACLE_ABI = "./contracts/0.79.3_compiled/dex/Oracle.abi.json"
EVENTLIST_ABI = "./contracts/0.79.3_compiled/dex/OracleEventList.abi.json"
PRIVATE_NOTE_ABI = "./contracts/0.79.3_compiled/dex/PrivateNote.abi.json"
PMP_ABI = "./contracts/0.79.3_compiled/dex/PMP.abi.json"

# ── Key paths (regenerated each run) ─────────────────────────────────────────
ORACLE_KEY_PATH = "./tests/dex/shellnet_oracle.keys.json"
EPHEMERAL_KEY_PATH = "./tests/dex/shellnet_ephemeral.keys.json"
LOSER_KEY_PATH = "./tests/dex/shellnet_loser.keys.json"
NEW_OWNER_KEY_PATH = "./tests/dex/shellnet_new_owner.keys.json"

# ── Token types ───────────────────────────────────────────────────────────────
TOKEN_TYPE = 1         # NACKL
TOKEN_TYPE_SHELL = 2   # Shell (ECC_SHELL)
TOKEN_TYPE_ECC = 300   # Shell fee (ECC)

# ── Oracle / Event constants ─────────────────────────────────────────────────
# Random suffix added at runtime to avoid collisions between runs
ORACLE_NAME_PREFIX = "ShOracle_"
ORACLE2_NAME_PREFIX = "ShOracle2_"
EVENT_NAME = "Winner of match X"
EVENT_DESCRIBE = "Who will win match X"
EVENT_OUTCOMES = {1: "Team A", 2: "Team B"}
EVENT_DEADLINE = 2000000000
ORACLE_FEE = 100

# Pre-computed: tvm.hash(abi.encode(EVENT_NAME, EVENT_DEADLINE, EVENT_DESCRIBE, EVENT_OUTCOMES))
EVENT_ID = "0x67f17d97fb26ce706694339bf87ee24fe9c11752ff7ccc2a1fb56ea67e4e4e2f"

# ── Deposit / stake amounts ──────────────────────────────────────────────────
VAULT_DEPOSIT = 1_000_000_000
ECC_SHELL_DEPOSIT = 10_000_000_000
STAKE_AMOUNT = 200_000_000
STAKE_OUTCOME = 0
LOSING_OUTCOME = 1
COUPON_STAKE_AMOUNT = 10_000_000
PHASE5H_LOSING_BET = 10_000_000

# Deployer must cover all outcomes before full-set window.
DEPLOYER_SEED_AMOUNT = 10_000_000   # seed stake on LOSING_OUTCOME to satisfy deployer coverage

# ── ZK proof secret keys (generated randomly each run) ───────────────────────
SKCOMMIT = generate_random_sk()
SKCOMMIT_LOSER = generate_random_sk()

# ── Timing constants ─────────────────────────────────────────────────────────
PHASE2_STAKE_PERIOD = 120
PHASE3_STAKE_PERIOD = 300
PHASE5_STAKE_PERIOD = 300   # shellnet is slower; regular window = 30s
PHASE5H_STAKE_PERIOD = 300

# ── Contract constants ───────────────────────────────────────────────────────
BET_TYPE_CLEAN = 0
BET_TYPE_DEBT = 1
BET_TYPE_COUPON = 2
NACKL_COUPON_VALUE = 100_000_000_000
FULL_PERCENT = 10000
FEE_PERCENT = 1


def log(title, data):
    print(f"\n=== {title} ===")
    print(data)
    print("===")


# ══════════════════════════════════════════════════════════════════════════════
# Assertion helpers
# ══════════════════════════════════════════════════════════════════════════════

def get_pn_balance(pn_details, token_type):
    bal = pn_details.get("balance", {})
    val = bal.get(str(token_type), bal.get(token_type, 0))
    return int(val) if val is not None else 0


def get_pmp_outcome_pool(pmp_details, outcome_id, bet_type):
    pools = pmp_details.get("typedOutcomePools", {})
    o = pools.get(str(outcome_id), pools.get(outcome_id, {}))
    if isinstance(o, dict):
        v = o.get(str(bet_type), o.get(bet_type, 0))
        return int(v) if v is not None else 0
    return 0


def normalize_uint256(val):
    if isinstance(val, int):
        return val
    s = str(val).strip()
    if s.startswith(("0x", "0X")):
        return int(s, 16)
    try:
        return int(s)
    except ValueError:
        return int(s, 16)


def assert_not_busy(pn_details, msg="PN must not be busy"):
    busy = pn_details.get("busyAddress")
    assert not busy, f"{msg}; busyAddress={busy}"


def check_event_in_eventlist(eventlist_address, event_id, must_exist=True):
    out = common.run_getter(eventlist_address, EVENTLIST_ABI, "_events", {})
    events_map = (out.get("_events") or {}) if isinstance(out, dict) else {}
    target = normalize_uint256(event_id)
    found = None
    for k, v in events_map.items():
        try:
            if normalize_uint256(k) == target:
                found = v
                break
        except (ValueError, TypeError):
            continue
    if must_exist:
        assert found is not None, \
            f"Event {event_id} not found in EventList {eventlist_address}"
    else:
        assert found is None, \
            f"Event {event_id} should NOT be in EventList {eventlist_address}"
    return found


def check_version(address, abi, expected_contract_name):
    out = common.run_getter(address, abi, "getVersion", {})
    assert out is not None, f"getVersion() failed for {address}"
    ver = out.get("value0", "")
    name = out.get("value1", "")
    assert ver == "1.0.1", \
        f"{expected_contract_name} version mismatch: expected 1.0.1, got '{ver}'"
    assert name == expected_contract_name, \
        f"Contract name mismatch: expected '{expected_contract_name}', got '{name}'"
    log(f"getVersion {expected_contract_name}", f"{ver} / {name}")


def get_pn_stakes(pn_address):
    out = common.run_getter(pn_address, PRIVATE_NOTE_ABI, "_stakes", {})
    return (out.get("_stakes") or {}) if isinstance(out, dict) else {}


def assert_pmp_self_destructed(pmp_address, grace_sleep=3):
    if grace_sleep > 0:
        time.sleep(grace_sleep)
    active = common.is_account_active(pmp_address)
    assert not active, f"PMP {pmp_address} should have self-destructed but is still Active"
    log("PMP self-destructed", pmp_address)


# ══════════════════════════════════════════════════════════════════════════════
# Phase 1 helpers – Oracle & PrivateNote setup
# ══════════════════════════════════════════════════════════════════════════════

def deploy_oracle(oracle_pubkey, oracle_name):
    params = {"oraclePubkey": oracle_pubkey, "oracleName": oracle_name}
    out = common.call_contract(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                               EPHEMERAL_KEY_PATH, "deployOracle", params)
    log("deployOracle", out)
    time.sleep(5)


def get_oracle_address(oracle_name):
    out = common.run_getter(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                            "getOracleAddress", {"name": oracle_name})
    addr = out.get("oracleAddress") if isinstance(out, dict) else out
    log("oracle address", addr)
    common.wait_account_active(addr)
    time.sleep(1)
    assert common.is_account_active(addr), "Oracle not active"
    return addr


def get_eventlist_address(oracle_address, index=0):
    out = common.run_getter(oracle_address, ORACLE_ABI,
                            "getEventListAddress", {"index": index})
    addr = out.get("value0") if isinstance(out, dict) else out
    log(f"eventlist address (index={index})", addr)
    common.wait_account_active(addr)
    time.sleep(1)
    assert common.is_account_active(addr), "EventList not active"
    return addr


def add_event(eventlist_address, event_name=EVENT_NAME, oracle_fee=ORACLE_FEE,
              deadline=EVENT_DEADLINE, describe=EVENT_DESCRIBE,
              outcomes=None, trust_addr=None):
    if outcomes is None:
        outcomes = EVENT_OUTCOMES
    params = {
        "event_name": event_name,
        "oracle_fee": oracle_fee,
        "deadline": deadline,
        "describe": describe,
        "outcomeNames": outcomes,
        "trustAddr": trust_addr,
    }
    out = common.call_contract(eventlist_address, EVENTLIST_ABI,
                               ORACLE_KEY_PATH, "addEvent", params)
    log(f"addEvent ({event_name})", out)
    time.sleep(2)
    return out


def generate_proof(skcommit, token_type, value):
    out = common.execute_cmd(f"./halo2-proover {skcommit} {token_type} {value}")
    log("halo2-prover output", out)
    time.sleep(1)
    raw = [line for line in out.splitlines() if line.strip()][-1]
    pair = json.loads(json.loads(raw))
    dih = "0x" + pair["private_note_digest"]
    nullifier = "0x" + pair["private_note_digest"]
    return (pair["proof"], dih,
            int(pair["private_note_sum"]), int(pair["token_type"]), nullifier)


def deploy_private_note(proof, deposit_identifier_hash, value, token_type, ephemeral_pubkey):
    params = {
        "zkproof": proof,
        "deposit_identifier_hash": deposit_identifier_hash,
        "ephemeral_pubkey": ephemeral_pubkey,
        "value": value,
        "token_type": token_type,
    }
    out = common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                               EPHEMERAL_KEY_PATH, "deployPrivateNote", params)
    log("deployPrivateNote", out)
    time.sleep(5)


def get_private_note_address(deposit_identifier_hash):
    out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                            "getPrivateNoteAddress",
                            {"deposit_identifier_hash": deposit_identifier_hash})
    log("getPrivateNoteAddress", out)
    time.sleep(1)
    return out["privateNoteAddress"]


def send_tokens_to_root_pn(token_type, value):
    """Send ECC tokens to RootPN contract via giver."""
    params = {
        "dest": ROOT_PN_ADDRESS,
        "value": 2_000_000_000,
        "ecc": {token_type: value},
        "flag": 1,
    }
    out = common.call_contract(GIVER_ADDRESS, GIVER_ABI, GIVER_KEY_PATH,
                               "sendCurrencyWithFlag", params)
    log(f"sendCurrencyWithFlag token_type={token_type} value={value}", out)
    time.sleep(3)


def send_ecc_to_private_note(proof, nullifier_hash, deposit_identifier_hash, value):
    """Send ECC shell tokens from RootPN to an existing PrivateNote (ZK proof, not giver)."""
    params = {
        "proof": proof,
        "nullifier_hash": nullifier_hash,
        "deposit_identifier_hash": deposit_identifier_hash,
        "value": value,
    }
    out = common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                               EPHEMERAL_KEY_PATH, "sendEccShellToPrivateNote", params)
    log("sendEccShellToPrivateNote", out)
    time.sleep(8)


# ══════════════════════════════════════════════════════════════════════════════
# Phase 2/3 helpers – PMP lifecycle
# ══════════════════════════════════════════════════════════════════════════════

def deploy_pmp(pn_address, event_id, oracle_name, oracle_fee, token_type, index=0):
    params = {
        "event_id": event_id,
        "oracleFee": [oracle_fee],
        "token_type": token_type,
        "names": [oracle_name],
        "index": [index],
        "initialStakes": [DEPLOYER_SEED_AMOUNT, DEPLOYER_SEED_AMOUNT],
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "deployPMP", params)
    log("deployPMP", out)
    time.sleep(5)

    pmp_out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI, "getPMPAddress",
                                {"event_id": event_id,
                                 "names": [oracle_name],
                                 "token_type": token_type})
    pmp_address = pmp_out["pmpAddress"]
    log("PMP address", pmp_address)
    time.sleep(2)
    common.wait_account_active(pmp_address)
    return pmp_address


def get_pmp_details(pmp_address):
    return common.run_getter(pmp_address, PMP_ABI, "getDetails", {})


def wait_pmp_approved(pmp_address, max_wait=120):
    deadline = time.time() + max_wait
    while time.time() < deadline:
        details = get_pmp_details(pmp_address)
        if details:
            n = int(details.get("numberOfOracleEvents", 0))
            a = int(details.get("approvedOracleEvents", 0))
            if n > 0 and a >= n:
                time.sleep(8)  # Allow onInitialStakesAccepted callback to reach PN
                log("PMP oracle confirmed", pmp_address)
                return details
        time.sleep(3)
    raise TimeoutError(f"PMP {pmp_address} oracle confirmation not received after {max_wait}s")


def oracle_submit_set_timings(pmp_address, result_start):
    params = {
        "resultStart": result_start,
    }
    out = common.call_contract(pmp_address, PMP_ABI,
                               ORACLE_KEY_PATH, "submitSetTimings", params)
    log("submitSetTimings", out)
    time.sleep(5)


def oracle_submit_resolve(pmp_address, outcome_id):
    params = {"outcomeId": outcome_id}
    out = common.call_contract(pmp_address, PMP_ABI,
                               ORACLE_KEY_PATH, "submitResolve", params)
    log("submitResolve", out)
    time.sleep(5)


def oracle_submit_cancel_event(pmp_address):
    out = common.call_contract(pmp_address, PMP_ABI,
                               ORACLE_KEY_PATH, "submitCancelEvent", {})
    log("submitCancelEvent", out)
    time.sleep(5)


def pn_set_stake(pn_address, event_id, oracle_list_hash, token_type,
                 outcome, amount, use_coupon=False, key_path=EPHEMERAL_KEY_PATH):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "outcome": outcome,
        "amount": amount,
        "use_coupon": use_coupon,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "setStake", params)
    log(f"setStake outcome={outcome} use_coupon={use_coupon}", out)
    time.sleep(5)


def pn_cancel_stake(pn_address, event_id, oracle_list_hash, token_type):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "cancelStake", params)
    log("cancelStake", out)
    time.sleep(5)


def pn_claim(pn_address, event_id, oracle_list_hash, token_type,
             key_path=EPHEMERAL_KEY_PATH):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "claim", params)
    log("claim", out)
    time.sleep(5)


def pn_change_owner(pn_address, new_pubkey, key_path=EPHEMERAL_KEY_PATH):
    params = {"new_pubkey": new_pubkey}
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "changeOwner", params)
    log("changeOwner", out)
    time.sleep(3)


def pn_withdraw_tokens(pn_address, dest_addr, token_type,
                       key_path=EPHEMERAL_KEY_PATH):
    params = {
        "flags": 1,
        "dest_wallet_addr": dest_addr,
        "token_type": token_type,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "withdrawTokens", params)
    log("withdrawTokens", out)
    time.sleep(5)


def get_pn_details(pn_address):
    return common.run_getter(pn_address, PRIVATE_NOTE_ABI, "getDetails", {})


def pn_generate_coupon(pn_address, token_type, key_path=EPHEMERAL_KEY_PATH):
    params = {"token_type": token_type}
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "generateCoupon", params)
    log("generateCoupon", out)
    time.sleep(3)
    return out


def pn_delete_stake(pn_address, event_id, oracle_list_hash, token_type,
                    key_path=EPHEMERAL_KEY_PATH):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               key_path, "deleteStake", params)
    log("deleteStake", out)
    time.sleep(2)
    return out


# ══════════════════════════════════════════════════════════════════════════════
# Phase 4 helpers – Oracle management
# ══════════════════════════════════════════════════════════════════════════════

def oracle_deploy_event_list(oracle_address, index):
    out = common.call_contract(oracle_address, ORACLE_ABI,
                               ORACLE_KEY_PATH, "deployEventList", {"index": index})
    log(f"deployEventList index={index}", out)
    time.sleep(5)


def oracle_delete_event(eventlist_address, event_id):
    out = common.call_contract(eventlist_address, EVENTLIST_ABI,
                               ORACLE_KEY_PATH, "deleteEvent", {"event_id": event_id})
    log("deleteEvent", out)
    time.sleep(3)


def oracle_withdraw_fees(oracle_address, dest_addr, amount):
    out = common.call_contract(oracle_address, ORACLE_ABI,
                               ORACLE_KEY_PATH, "withdrawFees",
                               {"to": dest_addr, "amount": amount})
    log("withdrawFees", out)
    time.sleep(3)


# ══════════════════════════════════════════════════════════════════════════════
# Main test
# ══════════════════════════════════════════════════════════════════════════════

def main():
    network = os.getenv("NETWORK", "shellnet.ackinacki.org")
    os.environ["NETWORK"] = network
    common.NETWORK = network
    common.set_config({"async_call": "false"})
    common.setup()
    time.sleep(1)

    # Random suffix per run to avoid oracle address collisions
    run_id = os.urandom(4).hex()
    ORACLE_NAME = ORACLE_NAME_PREFIX + run_id
    ORACLE2_NAME = ORACLE2_NAME_PREFIX + run_id

    print(f"Network:    {network}")
    print(f"Run ID:     {run_id}")
    print(f"Oracle1:    {ORACLE_NAME}")
    print(f"Oracle2:    {ORACLE2_NAME}")
    print(f"SK:         {SKCOMMIT}")
    print(f"SK_LOSER:   {SKCOMMIT_LOSER}")

    common.wait_account_active(ROOT_PN_ADDRESS)
    common.wait_account_active(ROOT_ORACLE_ADDRESS)

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 1 – Oracle & PrivateNote setup
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 1: Oracle & PrivateNote setup")

    # Generate fresh keys each run
    common.gen_keys(ORACLE_KEY_PATH)
    oracle_pubkey = common.read_public_key(ORACLE_KEY_PATH)
    if not oracle_pubkey.startswith("0x"):
        oracle_pubkey = "0x" + oracle_pubkey
    log("oracle pubkey", oracle_pubkey)

    common.gen_keys(EPHEMERAL_KEY_PATH)
    ephemeral_pubkey = common.read_public_key(EPHEMERAL_KEY_PATH)
    if not ephemeral_pubkey.startswith("0x"):
        ephemeral_pubkey = "0x" + ephemeral_pubkey
    log("ephemeral pubkey", ephemeral_pubkey)

    deploy_oracle(oracle_pubkey, ORACLE_NAME)
    oracle_address = get_oracle_address(ORACLE_NAME)

    check_version(oracle_address, ORACLE_ABI, "Oracle")

    eventlist_address = get_eventlist_address(oracle_address, index=0)
    check_version(eventlist_address, EVENTLIST_ABI, "OracleEventList")

    # Record EventList event count before addEvent
    events_before = common.run_getter(eventlist_address, EVENTLIST_ABI, "_events", {})
    events_map_before = (events_before.get("_events") or {}) if isinstance(events_before, dict) else {}
    n_events_before_add = len(events_map_before)
    log("Phase 1.3: events in EventList before addEvent", n_events_before_add)

    add_event(eventlist_address)

    event_info = check_event_in_eventlist(eventlist_address, EVENT_ID, must_exist=True)
    assert event_info["event_name"] == EVENT_NAME
    assert int(event_info["oracle_fee"]) == ORACLE_FEE
    log("Phase 1.4 PASSED: event in EventList", event_info["event_name"])

    # No giver on shellnet — skip send_tokens_to_root_pn.
    # RootPN already has ECC_SHELL from previous activity.

    # Generate ZK proof and deploy PrivateNote
    proof, dih, value, token_type, nullifier_hash = generate_proof(
        SKCOMMIT, TOKEN_TYPE, VAULT_DEPOSIT
    )

    deploy_private_note(proof, dih, value, token_type, ephemeral_pubkey)

    pn_address = get_private_note_address(dih)
    common.wait_account_active(pn_address)
    log("PrivateNote active", pn_address)

    check_version(pn_address, PRIVATE_NOTE_ABI, "PrivateNote")

    # Replenish RootPN ECC_SHELL via giver, then transfer to PrivateNote via ZK proof
    send_tokens_to_root_pn(TOKEN_TYPE_SHELL, ECC_SHELL_DEPOSIT)
    proof_sh, nullifier_sh, value_sh, _, _ = generate_proof(
        SKCOMMIT, TOKEN_TYPE_ECC, ECC_SHELL_DEPOSIT
    )
    send_ecc_to_private_note(proof_sh, nullifier_sh, dih, ECC_SHELL_DEPOSIT)

    pn0 = get_pn_details(pn_address)
    log("PrivateNote initial state", pn0)
    assert pn0 is not None, "PrivateNote details unavailable"

    pn0_nackl = get_pn_balance(pn0, TOKEN_TYPE)
    assert pn0_nackl == VAULT_DEPOSIT, \
        f"NACKL balance must be {VAULT_DEPOSIT}, got {pn0_nackl}"
    log("Phase 1.6 PASSED: PN NACKL balance after deposit", pn0_nackl)

    assert_not_busy(pn0, "PN must not be busy after deploy")

    stored_dih = normalize_uint256(pn0.get("depositIdentifierHash", 0))
    expected_dih = normalize_uint256(dih)
    assert stored_dih == expected_dih, \
        f"depositIdentifierHash mismatch: {stored_dih} != {expected_dih}"

    stored_epk = normalize_uint256(pn0.get("etherealPubkey", 0))
    expected_epk = normalize_uint256(ephemeral_pubkey)
    assert stored_epk == expected_epk, \
        f"etherealPubkey mismatch after deploy"

    print(">>> Phase 1 PASSED")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 2 – PMP Happy Path
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 2: PMP Happy Path (stake → resolve → claim win)")

    ev_before_pmp1 = check_event_in_eventlist(eventlist_address, EVENT_ID, must_exist=True)
    event_count_before_pmp1 = int(ev_before_pmp1["count"])

    pmp1_address = deploy_pmp(
        pn_address, EVENT_ID, ORACLE_NAME, ORACLE_FEE, TOKEN_TYPE, index=0
    )

    check_version(pmp1_address, PMP_ABI, "PMP")

    pmp1_approved = wait_pmp_approved(pmp1_address)
    oracle_list_hash = pmp1_approved["oracle_list_hash"]
    log("oracle_list_hash", oracle_list_hash)

    pmp1_pre = get_pmp_details(pmp1_address)
    assert not pmp1_pre["approved"], "PMP must NOT be approved before submitSetTimings"
    assert int(pmp1_pre.get("numOutcomes", 0)) == 2
    assert int(pmp1_pre.get("token_type", -1)) == TOKEN_TYPE
    assert not pmp1_pre.get("isCancelled")
    assert pmp1_pre.get("resolvedOutcome") is None
    assert int(pmp1_pre.get("totalPool", 0)) == 2 * DEPLOYER_SEED_AMOUNT
    log("Phase 2.2 PASSED: PMP initial state verified (totalPool = 2*DS from initialStakes)", "")

    event_after_deploy = check_event_in_eventlist(eventlist_address, EVENT_ID, must_exist=True)
    count_after_pmp1 = int(event_after_deploy["count"])
    assert count_after_pmp1 == event_count_before_pmp1 + 1
    log("Phase 2.3 PASSED: event count after PMP deploy", count_after_pmp1)

    now = int(time.time())
    s_start  = now
    r_start  = now + PHASE2_STAKE_PERIOD

    oracle_submit_set_timings(pmp1_address, r_start)
    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s_end = s_start + (r_start - s_start) // 10

    pmp1_d = get_pmp_details(pmp1_address)
    assert pmp1_d["approved"], "PMP must be approved after submitSetTimings"
    log("Phase 2.4 PASSED: PMP timings set correctly", "")

    pn_before_stake2 = get_pn_details(pn_address)
    pn_bal_before_stake2 = get_pn_balance(pn_before_stake2, TOKEN_TYPE)
    assert pn_bal_before_stake2 == VAULT_DEPOSIT - 2 * DEPLOYER_SEED_AMOUNT

    stakes_before = get_pn_stakes(pn_address)
    assert len(stakes_before) == 1  # 1 initial stakes record from deployPMP

    pn_set_stake(
        pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE,
        STAKE_OUTCOME, STAKE_AMOUNT
    )

    pmp1_after_stake = get_pmp_details(pmp1_address)
    total_pool = int(pmp1_after_stake.get("totalPool", 0))
    log("PMP1 totalPool after stake", total_pool)
    assert total_pool > 0

    pn_after_stake2 = get_pn_details(pn_address)
    pn_bal_after_stake2 = get_pn_balance(pn_after_stake2, TOKEN_TYPE)
    assert pn_bal_after_stake2 == pn_bal_before_stake2 - STAKE_AMOUNT
    log("Phase 2.6 PASSED: PN balance decreased by STAKE_AMOUNT", pn_bal_after_stake2)

    assert_not_busy(pn_after_stake2)

    clean_pool2 = get_pmp_outcome_pool(pmp1_after_stake, STAKE_OUTCOME, BET_TYPE_CLEAN)
    assert clean_pool2 == DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT  # initial DS + regular SA
    assert total_pool == 2 * DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT  # 2 initial DS + regular SA
    log("Phase 2.8 PASSED: PMP pool updated correctly", "")

    stakes_after_stake2 = get_pn_stakes(pn_address)
    assert len(stakes_after_stake2) == 1

    # Wait for result window
    wait_for_result = s_end - int(time.time()) + 5
    if wait_for_result > 0:
        print(f"\n>>> Waiting {wait_for_result}s for result window...")
        time.sleep(wait_for_result)

    oracle_submit_resolve(pmp1_address, STAKE_OUTCOME)

    pmp1_d2 = get_pmp_details(pmp1_address)
    resolved = pmp1_d2.get("resolvedOutcome")
    assert resolved is not None
    assert int(resolved) == STAKE_OUTCOME
    assert not pmp1_d2.get("isCancelled")
    log("Phase 2.10 PASSED: resolvedOutcome == STAKE_OUTCOME", resolved)

    creator_fee2 = int(pmp1_d2.get("creatorFee", 0))
    assert creator_fee2 > 0  # non-zero: initial DS on losing side creates profit pool
    log("Phase 2.11 PASSED: creatorFee > 0 (initial stakes provide profit pool)", "")

    pn_claim(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE)

    pn_after_claim2 = get_pn_details(pn_address)
    pn_bal_after_claim2 = get_pn_balance(pn_after_claim2, TOKEN_TYPE)
    assert pn_bal_after_claim2 == pn_bal_before_stake2 + 2 * DEPLOYER_SEED_AMOUNT
    log("Phase 2.12 PASSED: PN balance restored after claim", pn_bal_after_claim2)

    assert_not_busy(pn_after_claim2)

    stakes_after_claim2 = get_pn_stakes(pn_address)
    assert len(stakes_after_claim2) == 0

    assert_pmp_self_destructed(pmp1_address, grace_sleep=3)

    print(">>> Phase 2 PASSED: stake → resolve → claim (win)")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 3 – PMP Cancel Path + Full-Set Stake
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 3: PMP Cancel Path + Full-Set Stake")

    time.sleep(5)

    pmp2_address = deploy_pmp(
        pn_address, EVENT_ID, ORACLE_NAME, ORACLE_FEE, TOKEN_TYPE, index=0
    )
    assert pmp2_address == pmp1_address, "PMP2 must have same address as PMP1"

    wait_pmp_approved(pmp2_address)

    pmp2_d = get_pmp_details(pmp2_address)
    oracle_list_hash2 = pmp2_d["oracle_list_hash"]
    assert oracle_list_hash2 == oracle_list_hash

    now3     = int(time.time())
    r3_start = now3 + PHASE3_STAKE_PERIOD

    oracle_submit_set_timings(pmp2_address, r3_start)
    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s3_end = now3 + (r3_start - now3) // 10

    pmp2_after_timings = get_pmp_details(pmp2_address)
    assert pmp2_after_timings["approved"]
    log("Phase 3.1 PASSED: PMP2 timings correct", "")

    pn_before_stake3 = get_pn_details(pn_address)
    pn_bal_before_stake3 = get_pn_balance(pn_before_stake3, TOKEN_TYPE)

    pn_set_stake(
        pn_address, EVENT_ID, oracle_list_hash2, TOKEN_TYPE,
        STAKE_OUTCOME, STAKE_AMOUNT
    )

    pmp2_after_regular = get_pmp_details(pmp2_address)
    assert int(pmp2_after_regular.get("totalPool", 0)) > 0

    clean_pool3a = get_pmp_outcome_pool(pmp2_after_regular, STAKE_OUTCOME, BET_TYPE_CLEAN)
    assert clean_pool3a == DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT  # initial DS + regular SA

    pn_after_reg_stake = get_pn_details(pn_address)
    pn_bal_after_reg3 = get_pn_balance(pn_after_reg_stake, TOKEN_TYPE)
    assert pn_bal_after_reg3 == pn_bal_before_stake3 - STAKE_AMOUNT
    assert_not_busy(pn_after_reg_stake)
    log("Phase 3.3 PASSED: PN balance after regular stake", pn_bal_after_reg3)

    # Cancel
    oracle_submit_cancel_event(pmp2_address)

    pmp2_cancelled = get_pmp_details(pmp2_address)
    assert pmp2_cancelled.get("isCancelled")
    assert pmp2_cancelled.get("resolvedOutcome") is None
    log("Phase 3.8 PASSED: PMP2 cancelled, not resolved", "")

    pn_cancel_stake(pn_address, EVENT_ID, oracle_list_hash2, TOKEN_TYPE)

    pn_after_cancel = get_pn_details(pn_address)
    pn_bal_after_cancel = get_pn_balance(pn_after_cancel, TOKEN_TYPE)
    assert pn_bal_after_cancel == pn_bal_before_stake3 + 2 * DEPLOYER_SEED_AMOUNT
    log("Phase 3.9 PASSED: PN balance restored after cancelStake", pn_bal_after_cancel)
    assert_not_busy(pn_after_cancel)

    stakes_after_cancel = get_pn_stakes(pn_address)
    assert len(stakes_after_cancel) == 0
    log("Phase 3.11 PASSED: PN stakes cleared after cancelStake", "")

    print(">>> Phase 3 PASSED: regular stake + cancel + refund")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 4 – Oracle management
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 4: Oracle management")

    oracle_deploy_event_list(oracle_address, index=1)
    eventlist1_address = get_eventlist_address(oracle_address, index=1)
    check_version(eventlist1_address, EVENTLIST_ABI, "OracleEventList")

    events1_before = common.run_getter(eventlist1_address, EVENTLIST_ABI, "_events", {})
    events1_map_before = (events1_before.get("_events") or {}) if isinstance(events1_before, dict) else {}
    assert len(events1_map_before) == 0

    add_event(
        eventlist1_address,
        event_name="Winner of match Y",
        oracle_fee=ORACLE_FEE,
        deadline=EVENT_DEADLINE,
        describe="Who will win match Y",
        outcomes={1: "Team C", 2: "Team D"},
    )

    events1_after = common.run_getter(eventlist1_address, EVENTLIST_ABI, "_events", {})
    events1_map_after = (events1_after.get("_events") or {}) if isinstance(events1_after, dict) else {}
    assert len(events1_map_after) == 1
    for k, v in events1_map_after.items():
        assert v.get("event_name") == "Winner of match Y"
        assert int(v.get("count", 0)) == 0
    log("Phase 4.3 PASSED: event stored in EventList1", "Winner of match Y")

    # deleteEvent from original EventList (event count == 0 after PMP2 cancel)
    oracle_delete_event(eventlist_address, EVENT_ID)
    log("deleteEvent", f"event {EVENT_ID} removed")

    # No withdraw on shellnet — skip oracle_withdraw_fees.

    print(">>> Phase 4 PASSED: deployEventList, addEvent, deleteEvent")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 5 – generateCoupon & coupon-stake flow
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 5: generateCoupon & coupon-stake flow")

    # Deploy oracle2
    oracle2_params = {"oraclePubkey": oracle_pubkey, "oracleName": ORACLE2_NAME}
    common.call_contract(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                         EPHEMERAL_KEY_PATH, "deployOracle", oracle2_params)
    time.sleep(5)

    oracle2_out = common.run_getter(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                                    "getOracleAddress", {"name": ORACLE2_NAME})
    oracle2_address = oracle2_out.get("oracleAddress") if isinstance(oracle2_out, dict) else oracle2_out
    log("oracle2 address", oracle2_address)
    common.wait_account_active(oracle2_address)

    check_version(oracle2_address, ORACLE_ABI, "Oracle")

    eventlist2_address = get_eventlist_address(oracle2_address, index=0)
    check_version(eventlist2_address, EVENTLIST_ABI, "OracleEventList")

    add_event(eventlist2_address)
    check_event_in_eventlist(eventlist2_address, EVENT_ID, must_exist=True)
    log("Phase 5.3 PASSED: event in oracle2 EventList", "")

    # Deploy loser PrivateNote (PN2)
    common.gen_keys(LOSER_KEY_PATH)
    loser_pubkey = common.read_public_key(LOSER_KEY_PATH)
    if not loser_pubkey.startswith("0x"):
        loser_pubkey = "0x" + loser_pubkey
    log("loser pubkey (PN2)", loser_pubkey)

    # No giver — skip send_tokens_to_root_pn for PN2.
    # RootPN already has ECC_SHELL.

    proof_l, dih_l, value_l, ttype_l, nullifier_l = generate_proof(
        SKCOMMIT_LOSER, TOKEN_TYPE, VAULT_DEPOSIT
    )
    deploy_private_note(proof_l, dih_l, value_l, ttype_l, loser_pubkey)

    pn2_address = get_private_note_address(dih_l)
    common.wait_account_active(pn2_address)
    log("PN2 (loser) active", pn2_address)

    pn2_initial = get_pn_details(pn2_address)
    pn2_nackl_initial = get_pn_balance(pn2_initial, TOKEN_TYPE)
    assert pn2_nackl_initial == VAULT_DEPOSIT
    assert_not_busy(pn2_initial)
    log("Phase 5.4 PASSED: PN2 initial balance", pn2_nackl_initial)

    # Replenish RootPN ECC_SHELL via giver, then transfer to PN2 via ZK proof
    send_tokens_to_root_pn(TOKEN_TYPE_SHELL, ECC_SHELL_DEPOSIT)
    proof_l_sh, null_l_sh, value_l_sh, _, _ = generate_proof(
        SKCOMMIT_LOSER, TOKEN_TYPE_ECC, ECC_SHELL_DEPOSIT
    )
    send_ecc_to_private_note(proof_l_sh, null_l_sh, dih_l, ECC_SHELL_DEPOSIT)

    pn2_after_ecc = get_pn_details(pn2_address)
    log("PN2 state after ECC funding", pn2_after_ecc)

    pn2_balance_map = pn2_after_ecc.get("balance", {})
    pn2_nackl_balance = int(pn2_balance_map.get(str(TOKEN_TYPE), 0))
    if pn2_nackl_balance == 0:
        pn2_nackl_balance = int(pn2_balance_map.get(TOKEN_TYPE, 0))
    log("PN2 NACKL balance (will stake all)", pn2_nackl_balance)
    assert pn2_nackl_balance > 0, "PN2 must have NACKL balance to stake"

    pn1_before_stake5 = get_pn_details(pn_address)
    pn1_bal_before_stake5 = get_pn_balance(pn1_before_stake5, TOKEN_TYPE)

    # Deploy PMP3 via PN1 using oracle2
    pmp3_address = deploy_pmp(
        pn_address, EVENT_ID, ORACLE2_NAME, ORACLE_FEE, TOKEN_TYPE, index=0
    )

    pmp3_approved = wait_pmp_approved(pmp3_address)
    oracle_list_hash3 = pmp3_approved["oracle_list_hash"]
    log("PMP3 oracle_list_hash", oracle_list_hash3)

    pmp3_pre = get_pmp_details(pmp3_address)
    assert not pmp3_pre["approved"]
    assert int(pmp3_pre.get("numOutcomes", 0)) == 2
    log("Phase 5.5 PASSED: PMP3 initial state", "")

    now5     = int(time.time())
    r5_start = now5 + PHASE5_STAKE_PERIOD

    oracle_submit_set_timings(pmp3_address, r5_start)
    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s5_end = now5 + (r5_start - now5) // 10

    pmp3_d = get_pmp_details(pmp3_address)
    assert pmp3_d["approved"]

    # PN1 stakes on winning side
    pn_set_stake(
        pn_address, EVENT_ID, oracle_list_hash3, TOKEN_TYPE,
        STAKE_OUTCOME, STAKE_AMOUNT
    )
    pmp3_after_p1 = get_pmp_details(pmp3_address)
    assert int(pmp3_after_p1.get("totalPool", 0)) > 0

    pn1_after_stake5 = get_pn_details(pn_address)
    pn1_bal_after_stake5 = get_pn_balance(pn1_after_stake5, TOKEN_TYPE)
    assert pn1_bal_after_stake5 == pn1_bal_before_stake5 - 2 * DEPLOYER_SEED_AMOUNT - STAKE_AMOUNT
    assert_not_busy(pn1_after_stake5)
    log("Phase 5.6 PASSED: PN1 balance after Phase 5 stake", pn1_bal_after_stake5)

    # PN2 stakes ALL NACKL on losing side
    pn_set_stake(
        pn2_address, EVENT_ID, oracle_list_hash3, TOKEN_TYPE,
        LOSING_OUTCOME, pn2_nackl_balance,
        key_path=LOSER_KEY_PATH
    )
    pmp3_after_p2 = get_pmp_details(pmp3_address)
    assert int(pmp3_after_p2.get("totalPool", 0)) > int(pmp3_after_p1.get("totalPool", 0))

    expected_pmp3_pool = 2 * DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT + pn2_nackl_balance
    actual_pmp3_pool = int(pmp3_after_p2.get("totalPool", 0))
    assert actual_pmp3_pool == expected_pmp3_pool
    log("Phase 5.7 PASSED: PMP3 pool after both stakes", actual_pmp3_pool)

    pn2_after_stake = get_pn_details(pn2_address)
    pn2_bal_after_stake = get_pn_balance(pn2_after_stake, TOKEN_TYPE)
    assert pn2_bal_after_stake == 0
    assert_not_busy(pn2_after_stake)
    log("Phase 5.8 PASSED: PN2 balance == 0 after staking all", "")

    # Wait for result window, resolve
    wait5 = s5_end - int(time.time()) + 5
    if wait5 > 0:
        print(f"\n>>> Waiting {wait5}s for PMP3 result window...")
        time.sleep(wait5)

    oracle_submit_resolve(pmp3_address, STAKE_OUTCOME)

    pmp3_resolved = get_pmp_details(pmp3_address)
    assert pmp3_resolved.get("resolvedOutcome") is not None
    assert int(pmp3_resolved.get("resolvedOutcome")) == STAKE_OUTCOME

    creator_fee5 = int(pmp3_resolved.get("creatorFee", 0))
    expected_creator_fee5 = (2 * DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT + pn2_nackl_balance) * FEE_PERCENT // FULL_PERCENT
    assert creator_fee5 > 0
    assert creator_fee5 == expected_creator_fee5
    log("Phase 5.10 PASSED: creatorFee > 0", creator_fee5)

    # PN2 claims (loser → 0 payout)
    pn_claim(pn2_address, EVENT_ID, oracle_list_hash3, TOKEN_TYPE,
             key_path=LOSER_KEY_PATH)

    pn2_after_lose = get_pn_details(pn2_address)
    pn2_nackl_after = get_pn_balance(pn2_after_lose, TOKEN_TYPE)
    assert pn2_nackl_after == 0
    assert_not_busy(pn2_after_lose)

    stakes_pn2_after_lose = get_pn_stakes(pn2_address)
    assert len(stakes_pn2_after_lose) == 0
    log("Phase 5.12 PASSED: PN2 stakes cleared after losing claim", "")

    # PN1 claims (winner) – PMP3 self-destructs
    pn_claim(pn_address, EVENT_ID, oracle_list_hash3, TOKEN_TYPE)
    pn1_after_win = get_pn_details(pn_address)

    pn1_bal_after_win5 = get_pn_balance(pn1_after_win, TOKEN_TYPE)
    assert pn1_bal_after_win5 > pn1_bal_before_stake5
    net_gain5 = pn1_bal_after_win5 - pn1_bal_before_stake5
    log("Phase 5.13 PASSED: PN1 net gain", net_gain5)
    assert net_gain5 > 0
    assert_not_busy(pn1_after_win)

    assert_pmp_self_destructed(pmp3_address, grace_sleep=3)

    # generateCoupon on PN2
    pn_generate_coupon(pn2_address, TOKEN_TYPE, key_path=LOSER_KEY_PATH)

    pn2_with_coupon = get_pn_details(pn2_address)
    assert_not_busy(pn2_with_coupon)
    pn2_bal_with_coupon = get_pn_balance(pn2_with_coupon, TOKEN_TYPE)
    assert pn2_bal_with_coupon == 0
    log("Phase 5.16 PASSED: generateCoupon called successfully", "")

    print(">>> Phase 5g PASSED: generateCoupon called successfully")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 5h – coupon stake flow
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 5h: coupon stake flow")

    time.sleep(5)

    pmp4_address = deploy_pmp(
        pn_address, EVENT_ID, ORACLE2_NAME, ORACLE_FEE, TOKEN_TYPE, index=0
    )
    assert pmp4_address == pmp3_address, "PMP4 must have same address as PMP3"

    pmp4_approved = wait_pmp_approved(pmp4_address)
    oracle_list_hash4 = pmp4_approved["oracle_list_hash"]
    assert oracle_list_hash4 == oracle_list_hash3

    now5h     = int(time.time())
    r5h_start = now5h + PHASE5H_STAKE_PERIOD

    oracle_submit_set_timings(pmp4_address, r5h_start)
    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s5h_end = now5h + (r5h_start - now5h) // 10

    pmp4_d = get_pmp_details(pmp4_address)
    assert pmp4_d["approved"]

    pn1_before_stake5h = get_pn_details(pn_address)
    pn1_bal_before_stake5h = get_pn_balance(pn1_before_stake5h, TOKEN_TYPE)

    # PN1 clean stake on winning side
    pn_set_stake(
        pn_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE,
        STAKE_OUTCOME, STAKE_AMOUNT
    )
    pmp4_after_p1 = get_pmp_details(pmp4_address)
    assert int(pmp4_after_p1.get("totalPool", 0)) == 2 * DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT

    # PN1 also stakes on losing side (profit budget for coupon)
    pn_set_stake(
        pn_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE,
        LOSING_OUTCOME, PHASE5H_LOSING_BET
    )
    pmp4_after_losing = get_pmp_details(pmp4_address)
    expected_pool_5h = 2 * DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT + PHASE5H_LOSING_BET
    assert int(pmp4_after_losing.get("totalPool", 0)) == expected_pool_5h
    assert_not_busy(get_pn_details(pn_address))

    # PN2 coupon stake on winning side
    pn_set_stake(
        pn2_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE,
        STAKE_OUTCOME, COUPON_STAKE_AMOUNT, use_coupon=True,
        key_path=LOSER_KEY_PATH
    )
    pmp4_after_coupon = get_pmp_details(pmp4_address)

    coupon_pool4 = get_pmp_outcome_pool(pmp4_after_coupon, STAKE_OUTCOME, BET_TYPE_COUPON)
    assert coupon_pool4 == COUPON_STAKE_AMOUNT
    log("Phase 5h.2 PASSED: PMP4 coupon pool", coupon_pool4)

    total_pool4 = int(pmp4_after_coupon.get("totalPool", 0))
    assert total_pool4 == expected_pool_5h
    log("Phase 5h.3 PASSED: PMP4 totalPool", total_pool4)

    pn2_after_coupon_stake = get_pn_details(pn2_address)
    assert_not_busy(pn2_after_coupon_stake)

    # Wait for result, resolve
    wait5h = s5h_end - int(time.time()) + 5
    if wait5h > 0:
        print(f"\n>>> Waiting {wait5h}s for PMP4 result window...")
        time.sleep(wait5h)

    oracle_submit_resolve(pmp4_address, STAKE_OUTCOME)

    pmp4_resolved = get_pmp_details(pmp4_address)
    assert pmp4_resolved.get("resolvedOutcome") is not None
    assert int(pmp4_resolved.get("resolvedOutcome")) == STAKE_OUTCOME

    creator_fee4 = int(pmp4_resolved.get("creatorFee", 0))
    assert creator_fee4 > 0
    log("Phase 5h.5b PASSED: PMP4 creatorFee > 0", creator_fee4)

    # PN2 claims coupon payout
    pn_claim(pn2_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE,
             key_path=LOSER_KEY_PATH)

    pn2_after_coupon_win = get_pn_details(pn2_address)
    pn2_bal_after_coupon_win = get_pn_balance(pn2_after_coupon_win, TOKEN_TYPE)
    assert pn2_bal_after_coupon_win > 0
    assert_not_busy(pn2_after_coupon_win)
    log("Phase 5h.6 PASSED: PN2 has payout after coupon win", pn2_bal_after_coupon_win)

    stakes_pn2_after_coupon = get_pn_stakes(pn2_address)
    assert len(stakes_pn2_after_coupon) == 0
    log("Phase 5h.7 PASSED: PN2 stakes cleared after coupon claim", "")

    # PN1 claims – PMP4 self-destructs
    pn_claim(pn_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE)
    pn1_after_pmp4 = get_pn_details(pn_address)
    pn1_bal_after_pmp4 = get_pn_balance(pn1_after_pmp4, TOKEN_TYPE)
    assert pn1_bal_after_pmp4 > pn1_bal_before_stake5h - PHASE5H_LOSING_BET - 1
    assert_not_busy(pn1_after_pmp4)
    log("Phase 5h.8 PASSED: PN1 balance after PMP4 win", pn1_bal_after_pmp4)

    assert_pmp_self_destructed(pmp4_address, grace_sleep=3)

    # deleteStake (idempotent)
    pn_delete_stake(pn2_address, EVENT_ID, oracle_list_hash4, TOKEN_TYPE,
                    key_path=LOSER_KEY_PATH)
    pn2_after_delete = get_pn_details(pn2_address)
    assert_not_busy(pn2_after_delete)
    log("Phase 5i.1 PASSED: deleteStake is idempotent", "")

    print(">>> Phase 5 PASSED: generateCoupon, coupon stake, coupon payout, deleteStake")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 6 – PrivateNote management: changeOwner
    # (withdrawTokens skipped on shellnet)
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 6: PrivateNote management (changeOwner)")

    # 6a. changeOwner
    common.gen_keys(NEW_OWNER_KEY_PATH)
    new_pubkey = common.read_public_key(NEW_OWNER_KEY_PATH)
    if not new_pubkey.startswith("0x"):
        new_pubkey = "0x" + new_pubkey

    pn_change_owner(pn_address, new_pubkey)

    pn_after_owner = get_pn_details(pn_address)
    stored_pk_new = normalize_uint256(pn_after_owner.get("etherealPubkey", 0))
    expected_pk_new = normalize_uint256(new_pubkey)
    assert stored_pk_new == expected_pk_new
    assert_not_busy(pn_after_owner)
    log("Phase 6.1 PASSED: etherealPubkey updated", hex(stored_pk_new))

    # Restore original key
    orig_pubkey_str = common.read_public_key(EPHEMERAL_KEY_PATH)
    if not orig_pubkey_str.startswith("0x"):
        orig_pubkey_str = "0x" + orig_pubkey_str
    pn_change_owner(pn_address, orig_pubkey_str, key_path=NEW_OWNER_KEY_PATH)

    pn_after_restore = get_pn_details(pn_address)
    stored_pk_restored = normalize_uint256(pn_after_restore.get("etherealPubkey", 0))
    expected_pk_orig = normalize_uint256(orig_pubkey_str)
    assert stored_pk_restored == expected_pk_orig
    assert_not_busy(pn_after_restore)
    log("Phase 6.2 PASSED: etherealPubkey restored", hex(stored_pk_restored))

    # No withdraw on shellnet — skip withdrawTokens and generateCoupon-after-withdraw check.

    print(">>> Phase 6 PASSED: changeOwner")

    # ══════════════════════════════════════════════════════════════════════════
    # Summary
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "=" * 64)
    print("ALL PHASES PASSED")
    print("=" * 64)
    print(f"Oracle1 address:     {oracle_address}")
    print(f"Oracle2 address:     {oracle2_address}")
    print(f"EventList0 address:  {eventlist_address}")
    print(f"EventList1 address:  {eventlist1_address}")
    print(f"EventList2 address:  {eventlist2_address}")
    print(f"PN1 address:         {pn_address}")
    print(f"PN2 address:         {pn2_address}")
    print(f"deposit_id_hash:     {dih}")
    print(f"oracle_list_hash:    {oracle_list_hash}")
    print(f"oracle_list_hash3:   {oracle_list_hash3}")
    print("=" * 64)


if __name__ == "__main__":
    main()
