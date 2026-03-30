"""
OrderBook & Split/Merge integration test.

Tests new DEX functionality:
  Split/Merge:
    - auto-freeze (after stakeEnd)
    - splitFullSet (collateral → proportional outcome tokens)
    - mergeFullSet (proportional outcome tokens → collateral)
  OrderBook:
    - auto-deploy via auto-freeze
    - placeOrder (buy/sell limit orders via PrivateNote)
    - cancelOrder (cancel and unlock tokens/collateral)
    - immediate matching (WASM: taker order matches resting maker on placement)
    - getDetails / getOrder getters

Test sequence:
  Phase 1  – Oracle & PrivateNote setup (reuses main_test pattern)
  Phase 2  – Deploy PMP, stake on both outcomes, set timings
  Phase 3  – auto-freeze → verify frozen state + OrderBook deployed
  Phase 4  – splitFullSet → verify outcome tokens received
  Phase 5  – placeOrder (sell) → verify → cancelOrder → verify
  Phase 6  – placeOrder (buy) → verify → cancelOrder → verify
  Phase 7  – Place sell + crossing buy → verify immediate matching + fills
  Phase 8  – mergeFullSet → verify collateral returned (must precede resolve)
  Phase 9  – Resolve PMP (within resolve window)
  Phase 10 – Cancel remaining buy order → verify balance
  Phase 11 – claim to clean up
"""

import json
import os
import random
import time
import sys

sys.path.append("tests")
from helper import common

# ── Contract addresses ─────────────────────────────────────────────────────────
GIVER_ADDRESS = "0:1111111111111111111111111111111111111111111111111111111111111111"
ROOT_PN_ADDRESS = "0:1010101010101010101010101010101010101010101010101010101010101010"
ROOT_ORACLE_ADDRESS = "0:1515151515151515151515151515151515151515151515151515151515151515"

# ── ABI paths ──────────────────────────────────────────────────────────────────
GIVER_ABI = "./contracts/giver/GiverV3.abi.json"
GIVER_KEY_PATH = "./tests/GiverV3.keys.json"
ROOT_PN_ABI = "./contracts/0.79.3_compiled/dex/RootPN.abi.json"
ROOT_ORACLE_ABI = "./contracts/0.79.3_compiled/dex/RootOracle.abi.json"
PRIVATE_NOTE_ABI = "./contracts/0.79.3_compiled/dex/PrivateNote.abi.json"
PMP_ABI = "./contracts/0.79.3_compiled/dex/PMP.abi.json"
ORACLE_ABI = "./contracts/0.79.3_compiled/dex/Oracle.abi.json"
EVENTLIST_ABI = "./contracts/0.79.3_compiled/dex/OracleEventList.abi.json"
ORDERBOOK_ABI = "./contracts/0.79.3_compiled/dex/OrderBook.abi.json"

# ── Key paths ──────────────────────────────────────────────────────────────────
EPHEMERAL_KEY_PATH = "./tests/dex/ephemeral.keys.json"
ORACLE_KEY_PATH = "./tests/dex/oracle.keys.json"

# ── Token types ────────────────────────────────────────────────────────────────
TOKEN_TYPE = 1        # NACKL
TOKEN_TYPE_SHELL = 2  # Shell
TOKEN_TYPE_ECC = 300  # Shell fee (ECC)

# ── Oracle / Event constants ───────────────────────────────────────────────────
ORACLE_NAME = "MyOracle"
EVENT_NAME = "Winner of match X"
EVENT_DESCRIBE = "Who will win match X"
EVENT_OUTCOMES = {1: "Team A", 2: "Team B"}
EVENT_DEADLINE = 2000000000
ORACLE_FEE = 100
EVENT_ID = "0x67f17d97fb26ce706694339bf87ee24fe9c11752ff7ccc2a1fb56ea67e4e4e2f"

# ── Deposit / stake amounts ────────────────────────────────────────────────────
VAULT_DEPOSIT = 200_000_000_000     # 200 NACKL
ECC_SHELL_DEPOSIT = 200_000_000_000 # 200 Shell
DEPLOYER_SEED_AMOUNT = 1_000_000_000  # 1 NACKL initial stake per outcome at deployPMP
STAKE_AMOUNT = 20_000_000_000       # 20 NACKL regular stake amount

# ── Split/Merge amounts ───────────────────────────────────────────────────────
SPLIT_COLLATERAL = 50_000_000_000   # 50 NACKL collateral for split

# ── OrderBook amounts (place/cancel tests) ──────────────────────────────────
ORDER_AMOUNT = 20_000_000_000       # 20 NACKL order amount (outcome tokens)
ORDER_PRICE_BPS = 5000              # 50% price in basis points

# ── settleEpoch amounts ─────────────────────────────────────────────────────
SETTLE_SELL_AMOUNT = 15_000_000_000 # 15 NACKL sell order for settle
SETTLE_BUY_AMOUNT = 20_000_000_000  # 20 NACKL buy order for settle (> sell → partial fill)
SETTLE_PRICE_BPS = 5000             # same price → clearing = 5000

# ── Timing constants ──────────────────────────────────────────────────────────
STAKE_PERIOD = 300  # seconds; staking window (regular window = 30s = 10%)
EPOCH_DURATION = 60  # seconds; epochDuration = r_end - s_end

# ── Contract constants ─────────────────────────────────────────────────────────
BET_TYPE_CLEAN = 0
FULL_PERCENT = 10000

# ── Trading fee constants (Hyperliquid Tier 0) ──────────────────────────────
FEE_DENOMINATOR = 100000
MAKER_FEE_RATE = 15    # 0.015%
TAKER_FEE_RATE = 45    # 0.045%

# ── Order flags ───────────────────────────────────────────────────────────────
FLAG_IOC       = 0x01
FLAG_FOK       = 0x02
FLAG_MARKET    = 0x04
FLAG_POST_ONLY = 0x08
FLAG_GTC       = 0x10

# ── ZK proof secret key ───────────────────────────────────────────────────────
def generate_random_sk():
    return os.urandom(31).hex() + format(random.randint(0, 0x2f), '02x')

SKCOMMIT = generate_random_sk()


def log(title, data):
    print(f"\n=== {title} ===")
    print(data)
    print("===")


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


def assert_not_busy(pn_details, msg="PN must not be busy"):
    busy = pn_details.get("busyAddress")
    assert not busy, f"{msg}; busyAddress={busy}"


def get_pn_stakes(pn_address):
    out = common.run_getter(pn_address, PRIVATE_NOTE_ABI, "_stakes", {})
    return (out.get("_stakes") or {}) if isinstance(out, dict) else {}


# ══════════════════════════════════════════════════════════════════════════════
# Phase 1 helpers – Oracle & PrivateNote setup
# ══════════════════════════════════════════════════════════════════════════════

def generate_oracle_pubkey():
    common.gen_keys(ORACLE_KEY_PATH)
    pub = common.read_public_key(ORACLE_KEY_PATH)
    if not pub.startswith("0x"):
        pub = "0x" + pub
    log("oracle pubkey", pub)
    time.sleep(1)
    return pub


def generate_ephemeral_pubkey():
    common.gen_keys(EPHEMERAL_KEY_PATH)
    pub = common.read_public_key(EPHEMERAL_KEY_PATH)
    if not pub.startswith("0x"):
        pub = "0x" + pub
    log("ephemeral pubkey", pub)
    time.sleep(1)
    return pub


def deploy_oracle(oracle_pubkey):
    params = {"oraclePubkey": oracle_pubkey, "oracleName": ORACLE_NAME}
    out = common.call_contract(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                               EPHEMERAL_KEY_PATH, "deployOracle", params)
    log("deployOracle", out)
    time.sleep(3)


def get_oracle_address():
    out = common.run_getter(ROOT_ORACLE_ADDRESS, ROOT_ORACLE_ABI,
                            "getOracleAddress", {"name": ORACLE_NAME})
    addr = out.get("oracleAddress") if isinstance(out, dict) else out
    log("oracle address", addr)
    common.wait_account_active(addr)
    time.sleep(1)
    return addr


def get_eventlist_address(oracle_address, index=0):
    out = common.run_getter(oracle_address, ORACLE_ABI,
                            "getEventListAddress", {"index": index})
    addr = out.get("value0") if isinstance(out, dict) else out
    log(f"eventlist address (index={index})", addr)
    common.wait_account_active(addr)
    time.sleep(1)
    return addr


def add_event(eventlist_address):
    params = {
        "event_name": EVENT_NAME,
        "oracle_fee": ORACLE_FEE,
        "deadline": EVENT_DEADLINE,
        "describe": EVENT_DESCRIBE,
        "outcomeNames": EVENT_OUTCOMES,
        "trustAddr": None,
    }
    out = common.call_contract(eventlist_address, EVENTLIST_ABI,
                               ORACLE_KEY_PATH, "addEvent", params)
    log("addEvent", out)
    time.sleep(2)


def send_tokens_to_root_pn(token_type, value):
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
    time.sleep(2)


def get_private_note_address(deposit_identifier_hash):
    out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                            "getPrivateNoteAddress",
                            {"deposit_identifier_hash": deposit_identifier_hash})
    log("getPrivateNoteAddress", out)
    time.sleep(1)
    return out["privateNoteAddress"]


def send_ecc_to_private_note(proof, nullifier_hash, deposit_identifier_hash, value):
    params = {
        "proof": proof,
        "nullifier_hash": nullifier_hash,
        "deposit_identifier_hash": deposit_identifier_hash,
        "value": value,
    }
    out = common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                               EPHEMERAL_KEY_PATH, "sendEccShellToPrivateNote", params)
    log("sendEccShellToPrivateNote", out)
    time.sleep(6)


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
    time.sleep(3)

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


def wait_pmp_approved(pmp_address, max_wait=90):
    deadline = time.time() + max_wait
    while time.time() < deadline:
        details = get_pmp_details(pmp_address)
        if details:
            n = int(details.get("numberOfOracleEvents", 0))
            a = int(details.get("approvedOracleEvents", 0))
            if n > 0 and a >= n:
                time.sleep(5)
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
    time.sleep(3)


def oracle_submit_resolve(pmp_address, outcome_id):
    params = {"outcomeId": outcome_id}
    out = common.call_contract(pmp_address, PMP_ABI,
                               ORACLE_KEY_PATH, "submitResolve", params)
    log("submitResolve", out)
    time.sleep(3)


def pn_set_stake(pn_address, event_id, oracle_list_hash, token_type,
                 outcome, amount):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "outcome": outcome,
        "amount": amount,
        "use_coupon": False,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "setStake", params)
    log(f"setStake outcome={outcome}", out)
    time.sleep(4)


def pn_claim(pn_address, event_id, oracle_list_hash, token_type):
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "claim", params)
    log("claim", out)
    time.sleep(4)


def get_pn_details(pn_address):
    return common.run_getter(pn_address, PRIVATE_NOTE_ABI, "getDetails", {})


# ══════════════════════════════════════════════════════════════════════════════
# Split/Merge helpers
# ══════════════════════════════════════════════════════════════════════════════



def pn_split_full_set(pn_address, event_id, oracle_list_hash, token_type, collateral):
    """PrivateNote.splitFullSet — split collateral into outcome tokens."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "collateral": collateral,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "splitFullSet", params)
    log("splitFullSet", out)
    time.sleep(4)


def pn_merge_full_set(pn_address, event_id, oracle_list_hash, token_type, amounts):
    """PrivateNote.mergeFullSet — merge outcome tokens back into collateral."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "amount": amounts,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "mergeFullSet", params)
    log("mergeFullSet", out)
    time.sleep(4)


# ══════════════════════════════════════════════════════════════════════════════
# OrderBook helpers
# ══════════════════════════════════════════════════════════════════════════════

def get_orderbook_address():
    """Compute OrderBook address from RootPN getter."""
    out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI,
                            "getOrderBookAddress",
                            {"event_id": EVENT_ID,
                             "names": [ORACLE_NAME],
                             "token_type": TOKEN_TYPE})
    addr = out.get("orderBookAddress") if isinstance(out, dict) else out
    log("OrderBook address", addr)
    return addr


def get_ob_details(ob_address):
    """Return OrderBook.getDetails() dict."""
    return common.run_getter(ob_address, ORDERBOOK_ABI, "getDetails", {})


def get_ob_order(ob_address, order_id):
    """Return OrderBook.getOrder(orderId) dict."""
    return common.run_getter(ob_address, ORDERBOOK_ABI, "getOrder",
                             {"orderId": order_id})


def pn_place_order(pn_address, event_id, oracle_list_hash, token_type,
                   outcome_id, is_buy, price_bps, amount, flags=0, min_amount=0, epoch_id=0):
    """PrivateNote.placeOrder — place a limit order on OrderBook."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "outcomeId": outcome_id,
        "isBuy": is_buy,
        "priceBps": price_bps,
        "amount": amount,
        "flags": flags,
        "minAmount": min_amount,
        "epochId": epoch_id,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "placeOrder", params)
    log(f"placeOrder outcome={outcome_id} isBuy={is_buy} price={price_bps} amount={amount} flags={flags}", out)
    time.sleep(4)


def pn_cancel_order(pn_address, event_id, oracle_list_hash, token_type, order_id):
    """PrivateNote.cancelOrder — cancel an order on OrderBook."""
    params = {
        "event_id": event_id,
        "oracle_list_hash": oracle_list_hash,
        "token_type": token_type,
        "orderId": order_id,
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI,
                               EPHEMERAL_KEY_PATH, "cancelOrder", params)
    log(f"cancelOrder orderId={order_id}", out)
    time.sleep(4)




# ══════════════════════════════════════════════════════════════════════════════
# Main test
# ══════════════════════════════════════════════════════════════════════════════

def main():
    common.set_config({"async_call": "false"})
    common.setup()
    time.sleep(1)

    print(f"SK: {SKCOMMIT}")

    common.wait_account_active(GIVER_ADDRESS)
    common.wait_account_active(ROOT_PN_ADDRESS)
    common.wait_account_active(ROOT_ORACLE_ADDRESS)

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 1 – Oracle & PrivateNote setup
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 1: Oracle & PrivateNote setup")

    oracle_pubkey = generate_oracle_pubkey()
    ephemeral_pubkey = generate_ephemeral_pubkey()

    deploy_oracle(oracle_pubkey)
    oracle_address = get_oracle_address()
    eventlist_address = get_eventlist_address(oracle_address, index=0)

    add_event(eventlist_address)

    # Fund RootPN
    send_tokens_to_root_pn(TOKEN_TYPE_SHELL, ECC_SHELL_DEPOSIT)
    send_tokens_to_root_pn("1", VAULT_DEPOSIT)

    # Generate ZK proof and deploy PrivateNote
    proof, dih, value, token_type, nullifier_hash = generate_proof(
        SKCOMMIT, TOKEN_TYPE, VAULT_DEPOSIT
    )

    deploy_private_note(proof, dih, value, token_type, ephemeral_pubkey)
    pn_address = get_private_note_address(dih)
    common.wait_account_active(pn_address)
    log("PrivateNote active", pn_address)

    # Send ECC shell tokens
    proof_sh, nullifier_sh, value_sh, _, _ = generate_proof(
        SKCOMMIT, TOKEN_TYPE_ECC, ECC_SHELL_DEPOSIT
    )
    send_ecc_to_private_note(proof_sh, nullifier_sh, dih, ECC_SHELL_DEPOSIT)

    pn0 = get_pn_details(pn_address)
    pn0_nackl = get_pn_balance(pn0, TOKEN_TYPE)
    log("PN initial NACKL balance", pn0_nackl)
    assert pn0_nackl >= VAULT_DEPOSIT, f"PN balance must be >= {VAULT_DEPOSIT}, got {pn0_nackl}"

    print(">>> Phase 1 PASSED")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 2 – Deploy PMP, stake, set timings
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 2: Deploy PMP, stake on both outcomes, set timings")

    pmp_address = deploy_pmp(
        pn_address, EVENT_ID, ORACLE_NAME, ORACLE_FEE, TOKEN_TYPE, index=0
    )

    pmp_approved = wait_pmp_approved(pmp_address)
    oracle_list_hash = pmp_approved["oracle_list_hash"]
    log("oracle_list_hash", oracle_list_hash)

    # Set timings with shorter periods for settleEpoch testing
    now = int(time.time())
    s_start = now
    r_start = now + STAKE_PERIOD

    oracle_submit_set_timings(pmp_address, r_start)

    pmp_d = get_pmp_details(pmp_address)
    assert pmp_d["approved"], "PMP must be approved after submitSetTimings"

    # Stake on outcome 0 (regular window = first 12s)
    pn_set_stake(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE, 0, STAKE_AMOUNT)

    # Stake on outcome 1 as well (for balanced pools)
    pn_set_stake(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE, 1, STAKE_AMOUNT)

    pmp_after_stakes = get_pmp_details(pmp_address)
    total_pool = int(pmp_after_stakes.get("totalPool", 0))
    log("PMP totalPool after stakes", total_pool)
    expected_total = 2 * DEPLOYER_SEED_AMOUNT + 2 * STAKE_AMOUNT
    assert total_pool >= expected_total, \
        f"totalPool must be >= {expected_total}, got {total_pool}"

    # Record PN balance after staking
    pn_after_stakes = get_pn_details(pn_address)
    pn_bal_after_stakes = get_pn_balance(pn_after_stakes, TOKEN_TYPE)
    log("PN balance after all stakes", pn_bal_after_stakes)

    print(">>> Phase 2 PASSED")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 3 – auto-freeze (wait for stakeEnd)
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 3: Wait for stakeEnd (freeze + OB deploy on first split)")

    # stakeEnd is computed by contract as stakeStart + (resultStart - stakeStart) / 10
    s_end = s_start + (r_start - s_start) // 10
    wait_freeze = s_end - int(time.time()) + 3
    if wait_freeze > 0:
        print(f">>> Waiting {wait_freeze}s for stakeEnd...")
        time.sleep(wait_freeze)

    print(">>> Phase 3 PASSED: stakeEnd reached")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 4 – splitFullSet
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 4: splitFullSet")

    pn_before_split = get_pn_details(pn_address)
    pn_bal_before_split = get_pn_balance(pn_before_split, TOKEN_TYPE)
    log("PN balance before split", pn_bal_before_split)
    assert pn_bal_before_split >= SPLIT_COLLATERAL, \
        f"PN needs >= {SPLIT_COLLATERAL} for split, has {pn_bal_before_split}"

    # Get stakes before split to compare after
    stakes_before_split = get_pn_stakes(pn_address)
    log("PN stakes before split", stakes_before_split)

    pn_split_full_set(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE, SPLIT_COLLATERAL)

    pn_after_split = get_pn_details(pn_address)
    pn_bal_after_split = get_pn_balance(pn_after_split, TOKEN_TYPE)
    assert_not_busy(pn_after_split, "PN must not be busy after split")

    # ── Check 4.1: PN balance decreased by SPLIT_COLLATERAL ───────────────
    assert pn_bal_after_split == pn_bal_before_split - SPLIT_COLLATERAL, \
        f"PN balance must decrease by {SPLIT_COLLATERAL}: " \
        f"expected {pn_bal_before_split - SPLIT_COLLATERAL}, got {pn_bal_after_split}"
    log("Phase 4.1 PASSED: PN balance decreased by collateral", pn_bal_after_split)

    # ── Check 4.2: Auto-freeze happened, PMP totalPool increased ─────────
    pmp_after_split = get_pmp_details(pmp_address)
    assert pmp_after_split.get("frozen") == True, "PMP must be auto-frozen after split"
    pool_after_split = int(pmp_after_split.get("totalPool", 0))
    base_total = int(pmp_after_split.get("baseTotalPool", 0))
    assert pool_after_split > total_pool, \
        f"PMP totalPool must increase after split: was {total_pool}, now {pool_after_split}"
    assert base_total >= expected_total, \
        f"baseTotalPool must be >= {expected_total}, got {base_total}"
    log("Phase 4.2 PASSED: PMP auto-frozen + totalPool increased",
        {"totalPool": pool_after_split, "baseTotalPool": base_total})

    # ── Check 4.3: OrderBook auto-deployed ─────────────────────────────────
    ob_out = common.run_getter(pmp_address, PMP_ABI, "getOrderBookAddress", {})
    ob_address = ob_out.get("orderBookAddress") if isinstance(ob_out, dict) else None
    assert ob_address is not None, "OrderBook must be deployed after first split"
    log("OrderBook address", ob_address)

    common.wait_account_active(ob_address)
    ob_details = get_ob_details(ob_address)
    assert ob_details is not None, "OrderBook getDetails must return data"
    ob_initial_next_id = int(ob_details.get("nextOrderId", 0))
    ob_initial_order_count = int(ob_details.get("orderCount", 0))
    assert ob_initial_next_id >= 1, "nextOrderId must be >= 1"
    log("OrderBook initial state", {"nextOrderId": ob_initial_next_id, "orderCount": ob_initial_order_count})

    # ── Check 4.4: PN stake amounts increased ─────────────────────────────
    stakes_after_split = get_pn_stakes(pn_address)
    log("PN stakes after split", stakes_after_split)
    assert len(stakes_after_split) > 0, "PN must have stake records after split"

    # Compute expected split amounts using frozen clean pools
    # δ_k = floor(F * M_k / T) where M_k = frozen clean pool
    pool_0 = DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT  # clean outcome 0 at freeze
    pool_1 = DEPLOYER_SEED_AMOUNT + STAKE_AMOUNT  # clean outcome 1 at freeze
    expected_delta_0 = (SPLIT_COLLATERAL * pool_0) // base_total
    expected_delta_1 = (SPLIT_COLLATERAL * pool_1) // base_total
    log("Expected split deltas", {"delta_0": expected_delta_0, "delta_1": expected_delta_1})

    print(">>> Phase 4 PASSED: splitFullSet + auto-freeze + OrderBook")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 5 – placeOrder (sell) + cancel
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 5: placeOrder (sell) + cancelOrder")

    # We have outcome tokens from the split. Place a sell order for outcome 0.
    sell_amount = min(ORDER_AMOUNT, expected_delta_0)
    if sell_amount == 0:
        sell_amount = 1  # minimal amount

    pn_place_order(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE,
                   0, False, ORDER_PRICE_BPS, sell_amount)

    pn_after_sell = get_pn_details(pn_address)
    assert_not_busy(pn_after_sell, "PN must not be busy after sell order placed")

    # ── Check 5.1: OrderBook has the order ──────────────────────────────
    ob_details_after = get_ob_details(ob_address)
    sell_order_id = ob_initial_next_id  # the order we just placed
    assert int(ob_details_after.get("nextOrderId", 0)) == ob_initial_next_id + 1, \
        f"nextOrderId must be {ob_initial_next_id + 1} after placing order"
    assert int(ob_details_after.get("orderCount", 0)) == ob_initial_order_count + 1, \
        f"orderCount must be {ob_initial_order_count + 1}"
    log("Phase 5.1 PASSED: OrderBook has new order", ob_details_after)

    # ── Check 5.2: Order details correct ────────────────────────────────
    order1 = get_ob_order(ob_address, sell_order_id)
    log("Order details", order1)
    assert order1 is not None, f"Order {sell_order_id} must exist"
    assert int(order1.get("outcomeId", -1)) == 0, "outcomeId must be 0"
    assert order1.get("isBuy") == False, "isBuy must be False"
    assert normalize_uint256(order1.get("priceBps", 0)) == ORDER_PRICE_BPS, \
        f"priceBps must be {ORDER_PRICE_BPS}"
    assert int(order1.get("amount", 0)) == sell_amount, \
        f"amount must be {sell_amount}"
    log("Phase 5.2 PASSED: Order details verified", "")

    # ── Cancel sell order ────────────────────────────────────────────────
    pn_cancel_order(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE, sell_order_id)

    pn_after_cancel = get_pn_details(pn_address)
    assert_not_busy(pn_after_cancel, "PN must not be busy after cancel")

    ob_after_cancel = get_ob_details(ob_address)
    assert int(ob_after_cancel.get("orderCount", 0)) == ob_initial_order_count, \
        f"orderCount must be {ob_initial_order_count} after cancel"

    stakes_after_cancel_sell = get_pn_stakes(pn_address)
    log("Phase 5.3 PASSED: Sell order placed and cancelled", stakes_after_cancel_sell)

    print(">>> Phase 5 PASSED")

    # ══════════════════════════════════════════════════════════════════════
    # Phase 6 – placeOrder (buy) + cancel
    # ══════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 6: placeOrder (buy) + cancelOrder")

    buy_amount = ORDER_AMOUNT
    buy_cost = (buy_amount * ORDER_PRICE_BPS) // FULL_PERCENT
    buy_max_fee = (buy_cost * TAKER_FEE_RATE) // FEE_DENOMINATOR
    buy_locked = buy_cost + buy_max_fee

    pn_before_buy = get_pn_details(pn_address)
    pn_bal_before_buy = get_pn_balance(pn_before_buy, TOKEN_TYPE)
    log("PN balance before buy order", pn_bal_before_buy)
    assert pn_bal_before_buy >= buy_locked, \
        f"PN needs >= {buy_locked} for buy order, has {pn_bal_before_buy}"

    pn_place_order(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE,
                   0, True, ORDER_PRICE_BPS, buy_amount)

    pn_after_buy = get_pn_details(pn_address)
    assert_not_busy(pn_after_buy, "PN must not be busy after buy order")

    # ── Check 6.1: PN balance decreased by cost + maxFee ─────────────
    pn_bal_after_buy = get_pn_balance(pn_after_buy, TOKEN_TYPE)
    assert pn_bal_after_buy == pn_bal_before_buy - buy_locked, \
        f"PN balance must decrease by buy locked {buy_locked}: " \
        f"expected {pn_bal_before_buy - buy_locked}, got {pn_bal_after_buy}"

    # ── Check 6.2: OrderBook has the buy order ───────────────────────
    ob_after_buy = get_ob_details(ob_address)
    buy_order_id = int(ob_after_buy.get("nextOrderId", 0)) - 1
    assert int(ob_after_buy.get("orderCount", 0)) == ob_initial_order_count + 1, \
        f"orderCount must be {ob_initial_order_count + 1} after buy order"

    # ── Check 6.3: Buy order details correct ─────────────────────────
    buy_order = get_ob_order(ob_address, buy_order_id)
    assert buy_order.get("isBuy") == True, "isBuy must be True"
    assert normalize_uint256(buy_order.get("priceBps", 0)) == ORDER_PRICE_BPS
    assert int(buy_order.get("amount", 0)) == buy_amount

    # ── Cancel buy order ─────────────────────────────────────────────
    pn_cancel_order(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE,
                    buy_order_id)

    pn_after_cancel_buy = get_pn_details(pn_address)
    assert_not_busy(pn_after_cancel_buy, "PN must not be busy after cancel buy")

    pn_bal_after_cancel_buy = get_pn_balance(pn_after_cancel_buy, TOKEN_TYPE)
    assert pn_bal_after_cancel_buy == pn_bal_before_buy, \
        f"PN balance must be restored to {pn_bal_before_buy} after cancel buy, " \
        f"got {pn_bal_after_cancel_buy}"
    log("Phase 6 PASSED: Buy order placed and cancelled", pn_bal_after_cancel_buy)

    print(">>> Phase 6 PASSED")

    # ══════════════════════════════════════════════════════════════════════
    # Phase 7 – Place sell + crossing buy → verify immediate matching
    # ══════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 7: Place crossing orders + verify immediate matching")

    # Fee math for Phase 7 checks (both orders are limit, flags=0 → maker rate)
    trade_amount      = SETTLE_SELL_AMOUNT  # min(sell=15M, buy=20M)
    clearing_price    = SETTLE_PRICE_BPS    # maker's price (sell placed first)
    sell_notional     = (trade_amount * clearing_price) // FULL_PERCENT
    sell_fee          = (sell_notional * MAKER_FEE_RATE) // FEE_DENOMINATOR
    sell_net          = sell_notional - sell_fee
    buy_notional      = sell_notional
    buy_fee           = (buy_notional * MAKER_FEE_RATE) // FEE_DENOMINATOR
    buy_deficit       = buy_fee  # refund=0 since buy_price == clearing_price
    settle_buy_cost   = (SETTLE_BUY_AMOUNT * SETTLE_PRICE_BPS) // FULL_PERCENT
    settle_buy_max_fee = (settle_buy_cost * TAKER_FEE_RATE) // FEE_DENOMINATOR
    settle_buy_locked = settle_buy_cost + settle_buy_max_fee
    expected_remaining = SETTLE_BUY_AMOUNT - trade_amount

    pn_before_settle = get_pn_details(pn_address)
    pn_bal_before_settle = get_pn_balance(pn_before_settle, TOKEN_TYPE)
    log("PN balance before crossing orders", pn_bal_before_settle)

    # Place sell order: outcome 0, price 5000, 15M tokens
    assert expected_delta_0 >= SETTLE_SELL_AMOUNT, \
        f"Need {SETTLE_SELL_AMOUNT} outcome tokens for sell, have {expected_delta_0}"

    ob_before_sell = get_ob_details(ob_address)
    sell_order_id = int(ob_before_sell.get("nextOrderId", 0))

    pn_place_order(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE,
                   0, False, SETTLE_PRICE_BPS, SETTLE_SELL_AMOUNT)

    pn_after_sell = get_pn_details(pn_address)
    assert_not_busy(pn_after_sell, "PN must not be busy after sell order")

    # ── Check 7.1: sell rests in book ────────────────────────────────
    ob_after_sell = get_ob_details(ob_address)
    assert int(ob_after_sell.get("orderCount", 0)) == ob_initial_order_count + 1, \
        f"orderCount must be {ob_initial_order_count + 1} after sell"
    sell_order = get_ob_order(ob_address, sell_order_id)
    assert int(sell_order.get("amount", 0)) == SETTLE_SELL_AMOUNT, \
        "sell order must rest in book at full amount"
    log("Phase 7.1 PASSED: sell order resting", {"id": sell_order_id})

    # Place buy order: outcome 0, price 5000, 20M — crosses with the sell
    pn_bal_after_sell = get_pn_balance(pn_after_sell, TOKEN_TYPE)
    assert pn_bal_after_sell >= settle_buy_locked, \
        f"PN needs >= {settle_buy_locked} for buy, has {pn_bal_after_sell}"
    buy_order_id = sell_order_id + 1

    pn_place_order(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE,
                   0, True, SETTLE_PRICE_BPS, SETTLE_BUY_AMOUNT)

    pn_after_buy = get_pn_details(pn_address)
    assert_not_busy(pn_after_buy, "PN must not be busy after buy order")

    # ── Check 7.2: sell fully filled (removed), buy remainder rests ──
    ob_after_match = get_ob_details(ob_address)
    assert int(ob_after_match.get("orderCount", 0)) == ob_initial_order_count + 1, \
        "orderCount must be ob_initial+1: sell removed, buy remainder added"
    log("Phase 7.2 PASSED: orderCount after matching", ob_after_match.get("orderCount"))

    # ── Check 7.3: sell order removed from book ───────────────────────
    try:
        get_ob_order(ob_address, sell_order_id)
        assert False, f"Sell order {sell_order_id} should be removed after full fill"
    except Exception:
        log("Phase 7.3 PASSED: sell order removed (fully filled)", sell_order_id)

    # ── Check 7.4: buy order has expected remainder ───────────────────
    buy_order_after = get_ob_order(ob_address, buy_order_id)
    actual_remaining = int(buy_order_after.get("amount", 0))
    assert actual_remaining == expected_remaining, \
        f"Buy remainder must be {expected_remaining}, got {actual_remaining}"
    log("Phase 7.4 PASSED: buy order partially filled",
        {"remaining": actual_remaining})

    # ── Check 7.5: PN balance reflects immediate fill callbacks ──────
    # Locked: settle_buy_locked (cost + maxFee). Fill returns: feeRefund (maxFee - actualFee).
    # Net effect: -cost + sell_net - buy_fee (maxFee cancels out).
    pn_bal_after_fills = get_pn_balance(pn_after_buy, TOKEN_TYPE)
    expected_bal_after_fills = pn_bal_before_settle - settle_buy_cost + sell_net - buy_deficit
    assert pn_bal_after_fills == expected_bal_after_fills, \
        f"PN balance after fills must be {expected_bal_after_fills}, got {pn_bal_after_fills}"
    log("Phase 7.5 PASSED: PN balance after immediate fills",
        {"balance": pn_bal_after_fills,
         "sell_net": sell_net, "buy_deficit": buy_deficit})

    # ── Check 7.6: OrderBook fee tracking ────────────────────────────
    total_fees = sell_fee + buy_fee
    ob_fees = get_ob_details(ob_address)
    ob_maker_fees = int(ob_fees.get("totalMakerFees", 0))
    ob_taker_fees = int(ob_fees.get("totalTakerFees", 0))
    assert ob_maker_fees == total_fees, \
        f"totalMakerFees must be {total_fees}, got {ob_maker_fees}"
    assert ob_taker_fees == 0, \
        f"totalTakerFees must be 0 (no taker flags), got {ob_taker_fees}"
    log("Phase 7.6 PASSED: OB fee tracking",
        {"makerFees": ob_maker_fees, "expected": total_fees})

    print(">>> Phase 7 PASSED")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 8 – mergeFullSet (must happen before resolve)
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 8: mergeFullSet")

    pn_before_merge = get_pn_details(pn_address)
    pn_bal_before_merge = get_pn_balance(pn_before_merge, TOKEN_TYPE)
    stakes_before_merge = get_pn_stakes(pn_address)
    log("PN balance before merge", pn_bal_before_merge)
    log("PN stakes before merge", stakes_before_merge)

    # Send exact split amounts as upper-bound. PMP auto-clamps to max
    # proportional set (amounts may differ from base ratios after OB trades).
    merge_amounts = [expected_delta_0, expected_delta_1]
    log("Merge amounts", merge_amounts)

    pn_merge_full_set(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE, merge_amounts)

    pn_after_merge = get_pn_details(pn_address)
    pn_bal_after_merge = get_pn_balance(pn_after_merge, TOKEN_TYPE)
    assert_not_busy(pn_after_merge, "PN must not be busy after merge")

    # ── Check 8.1: PN balance increased by collateral ──────────────────────
    merge_collateral_returned = pn_bal_after_merge - pn_bal_before_merge
    log("Merge collateral returned", merge_collateral_returned)
    assert merge_collateral_returned > 0, \
        f"Merge must return positive collateral, got {merge_collateral_returned}"
    # After OB trades, outcome-0 tokens were partially sold, so merge is clamped
    # by the smaller outcome pool. Returned collateral <= SPLIT_COLLATERAL.
    assert merge_collateral_returned <= SPLIT_COLLATERAL, \
        f"Merge collateral must be <= {SPLIT_COLLATERAL}, got {merge_collateral_returned}"
    log("Phase 8.1 PASSED: collateral returned from merge", merge_collateral_returned)

    # ── Check 8.2: PMP totalPool decreased ─────────────────────────────────
    pmp_after_merge = get_pmp_details(pmp_address)
    pool_after_merge = int(pmp_after_merge.get("totalPool", 0))
    assert pool_after_merge < pool_after_split, \
        f"PMP pool must decrease after merge: was {pool_after_split}, now {pool_after_merge}"
    log("Phase 8.2 PASSED: PMP totalPool decreased after merge", pool_after_merge)

    print(">>> Phase 8 PASSED: mergeFullSet")

    # ══════════════════════════════════════════════════════════════════════
    # Phase 9 – Resolve PMP (within resolve window)
    # ══════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 9: Resolve PMP")

    # Resolve window is [r_start, r_end]. Must resolve before r_end.
    now_ts = int(time.time())
    if now_ts < r_start:
        wait_resolve = r_start - now_ts + 2
        print(f">>> Waiting {wait_resolve}s for resolve window...")
        time.sleep(wait_resolve)

    oracle_submit_resolve(pmp_address, 0)

    pmp_resolved = get_pmp_details(pmp_address)
    assert pmp_resolved.get("resolvedOutcome") is not None, "PMP must be resolved"
    assert int(pmp_resolved.get("resolvedOutcome")) == 0, "Resolved to outcome 0"
    log("Phase 9 PASSED: PMP resolved to outcome 0", "")

    # Re-read PN balance after resolve (includes creator fee from PMP)
    pn_after_resolve = get_pn_details(pn_address)
    pn_bal_after_resolve = get_pn_balance(pn_after_resolve, TOKEN_TYPE)
    log("PN balance after resolve", pn_bal_after_resolve)

    # Extract creator fee paid by PMP at resolve
    creator_fee = int(pmp_resolved.get("creatorFee", 0))
    log("PMP creator fee at resolve", creator_fee)
    assert pn_bal_after_resolve == pn_bal_after_merge + creator_fee, \
        f"PN balance after resolve must be {pn_bal_after_merge + creator_fee}, got {pn_bal_after_resolve}"
    log("Phase 9.1 PASSED: creator fee received", creator_fee)

    print(">>> Phase 9 PASSED")

    # ══════════════════════════════════════════════════════════════════════
    # Phase 10 – Cancel remaining buy order + verify balance
    # ══════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 10: Cancel remaining buy order")

    # Fills already happened in Phase 7 (immediate matching).
    # Only the 5M remainder of the buy is still in the book.
    # Fee reserve was fully consumed/refunded on partial fill (onOrderFilled deletes it).
    # Cancel returns only the remaining cost, no fee reserve.
    cancel_refund = (expected_remaining * SETTLE_PRICE_BPS) // FULL_PERCENT

    pn_cancel_order(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE, buy_order_id)

    pn_after_cancel = get_pn_details(pn_address)
    assert_not_busy(pn_after_cancel, "PN must not be busy after cancel remaining buy")

    pn_bal_after_cancel = get_pn_balance(pn_after_cancel, TOKEN_TYPE)
    expected_bal_after_cancel = pn_bal_after_resolve + cancel_refund
    assert pn_bal_after_cancel == expected_bal_after_cancel, \
        f"PN balance after cancel must be {expected_bal_after_cancel}, got {pn_bal_after_cancel}"

    ob_after_cancel = get_ob_details(ob_address)
    assert int(ob_after_cancel.get("orderCount", 0)) == ob_initial_order_count, \
        "orderCount must be back to initial after cancel"

    log("Phase 10 PASSED: remaining buy cancelled",
        {"cancel_refund": cancel_refund, "balance": pn_bal_after_cancel})

    print(">>> Phase 10 PASSED")

    # ══════════════════════════════════════════════════════════════════════════
    # Phase 11 – Claim
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n>>> Phase 11: Claim")

    pn_claim(pn_address, EVENT_ID, oracle_list_hash, TOKEN_TYPE)

    pn_final = get_pn_details(pn_address)
    pn_bal_final = get_pn_balance(pn_final, TOKEN_TYPE)
    log("PN final balance", pn_bal_final)
    assert_not_busy(pn_final, "PN must not be busy at end")

    print(">>> Phase 11 PASSED: Claimed")

    # ══════════════════════════════════════════════════════════════════════════
    # Summary
    # ══════════════════════════════════════════════════════════════════════════
    print("\n\n" + "=" * 60)
    print("  ALL ORDERBOOK & SPLIT/MERGE TESTS PASSED")
    print("  (including immediate matching with WASM engine)")
    print("=" * 60)


if __name__ == "__main__":
    main()
