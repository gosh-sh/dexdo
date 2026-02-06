import json
import time
import sys
import secrets

sys.path.append("tests")
from helper import common

GIVER_ADDRESS = "0:1111111111111111111111111111111111111111111111111111111111111111"
GIVER_ABI = "./contracts/giver/GiverV3.abi.json"
GIVER_KEY_PATH = "./tests/GiverV3.keys.json"

ROOT_PN_ADDRESS = "0:1010101010101010101010101010101010101010101010101010101010101010"

ROOT_PN_ABI = "./contracts/dex/RootPN.abi.json"
PRIVATE_NOTE_ABI = "./contracts/dex/PrivateNote.abi.json"

ORACLE_TEST_PATH = "./tests/dex/oracle_test.py"
EPHEMERAL_KEY_PATH = "./tests/dex/ephemeral.keys.json"

ECC_KEY = "1"
TOKEN_TYPE = 1
TOKEN_TYPE_SHELL = 2
TOKEN_TYPE_ECC = 300
VAULT_DEPOSIT = 1000
ECC_SHELL_DEPOSIT = 10_000_000_000

ORACLE_NAME = "MyOracle"
EVENT_ID = "0x67f17d97fb26ce706694339bf87ee24fe9c11752ff7ccc2a1fb56ea67e4e4e2f"
ORACLE_FEE = 100

def log(title, data):
    print(f"\n=== {title} ===")
    print(data)
    print("===")

def generate_skcommit():
    sk = secrets.token_hex(32)
    log("skcommit", sk)
    time.sleep(1)
    return sk

def generate_ephemeral_pubkey():
    common.gen_keys(EPHEMERAL_KEY_PATH)
    pub = common.read_public_key(EPHEMERAL_KEY_PATH)
    if not pub.startswith("0x"):
        pub = "0x" + pub
    log("ephemeral pubkey", pub)
    time.sleep(1)
    return pub

def ensure_account_active(address):
    out = common.execute_cli_cmd(f"account {address}")
    log(f"account {address}", out)
    time.sleep(1)

def send_tokens_to_root_pn(token_type, value):
    params = {
        "dest": ROOT_PN_ADDRESS,
        "value": 2_000_000_000,
        "ecc": {token_type: value},
        "flag": 1
    }
    out = common.call_contract(GIVER_ADDRESS, GIVER_ABI, GIVER_KEY_PATH, "sendCurrencyWithFlag", params)
    log("sendCurrencyWithFlag", out)
    time.sleep(3)

def generate_proof(skcommit, token_type, value):
    out = common.execute_cmd(f"./halo2-proover {skcommit} {token_type} {value}")
    log("halo2-prover output", out)
    time.sleep(1)
    raw = [l for l in out.splitlines() if l.strip()][-1]
    pair = json.loads(json.loads(raw))
    deposit_identifier_hash = "0x" + pair["private_note_digest"]
    nullifier_hash = "0x" + pair["private_note_digest"]
    return pair["proof"], deposit_identifier_hash, int(pair["private_note_sum"]), int(pair["token_type"]), nullifier_hash

def deploy_private_note(proof, deposit_identifier_hash, value, token_type, ephemeral_pubkey):
    params = {
        "zkproof": proof,
        "deposit_identifier_hash": deposit_identifier_hash,
        "ethemeral_pubkey": ephemeral_pubkey,
        "value": value,
        "token_type": token_type
    }
    out = common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI, EPHEMERAL_KEY_PATH, "deployPrivateNote", params)
    log("deployPrivateNote", out)
    time.sleep(1)

def get_private_note_address(deposit_identifier_hash):
    out = common.run_getter(ROOT_PN_ADDRESS, ROOT_PN_ABI, "getPrivateNoteAddress", {"deposit_identifier_hash": deposit_identifier_hash})
    log("getPrivateNoteAddress", out)
    time.sleep(1)
    return out["privateNoteAddress"]

def send_ecc_to_private_note(proof, nullifier_hash, deposit_identifier_hash, value):
    params = {
        "proof": proof,
        "nullifier_hash": nullifier_hash,
        "deposit_identifier_hash": deposit_identifier_hash,
        "value": value
    }
    out = common.call_contract(ROOT_PN_ADDRESS, ROOT_PN_ABI, EPHEMERAL_KEY_PATH, "sendEccShellToPrivateNote", params)
    log("sendEccShellToPrivateNote", out)
    time.sleep(6)

def deploy_pmp_contract(pn_address):
    params = {
        "event_id": EVENT_ID,
        "oracleFee": [ORACLE_FEE],
        "token_type": TOKEN_TYPE,
        "names": [ORACLE_NAME],
        "index": [0]
    }
    out = common.call_contract(pn_address, PRIVATE_NOTE_ABI, EPHEMERAL_KEY_PATH, "deployPMP", params)
    log("deployPMP", out)
    time.sleep(2)

    pmp_address = common.run_getter(
        ROOT_PN_ADDRESS,
        ROOT_PN_ABI,
        "getPMPAddress",
        {
            "event_id": EVENT_ID,
            "names": [ORACLE_NAME],
            "token_type": TOKEN_TYPE
        }
    )
    pmp_address = pmp_address["pmpAddress"]
    log("computed PMP address", pmp_address)
    time.sleep(10)

    ensure_account_active(pmp_address)
    return pmp_address


def run_oracle_test():
    out = common.execute_cmd(f"python3 {ORACLE_TEST_PATH}")
    log("oracle_test.py output", out)
    time.sleep(1)

def get_private_note_balance(address):
    out = common.execute_cli_cmd(f"account {address}")
    log(f"account {address}", out)
    time.sleep(1)
    return out.get("ecc_balance")

def main():
    common.set_config({"async_call": "false"})
    common.setup()
    time.sleep(1)

    ensure_account_active(GIVER_ADDRESS)
    ensure_account_active(ROOT_PN_ADDRESS)

    skcommit = "e2559befd891c2c1ee1175f5c86e8908d6c06acbe13e13069ec14158f18de40c"
    ephemeral_pubkey = generate_ephemeral_pubkey()

    send_tokens_to_root_pn(TOKEN_TYPE_SHELL, ECC_SHELL_DEPOSIT)
    send_tokens_to_root_pn(ECC_KEY, VAULT_DEPOSIT)

    proof, dih, value, token_type, nullifier_hash = generate_proof(skcommit, TOKEN_TYPE, VAULT_DEPOSIT)
    deploy_private_note(proof, dih, value, token_type, ephemeral_pubkey)

    pn_address = get_private_note_address(dih)
    ensure_account_active(pn_address)

    proof, nullifier_hash, value, token_type, _ = generate_proof(skcommit, TOKEN_TYPE_ECC, ECC_SHELL_DEPOSIT)
    send_ecc_to_private_note(proof, nullifier_hash, dih, ECC_SHELL_DEPOSIT)

    balance = get_private_note_balance(pn_address)
    assert balance is not None and balance != {}

    run_oracle_test()

    deploy_pmp_contract(pn_address)

    print("\nFINAL RESULT")
    print("PrivateNote address:", pn_address)
    print("PrivateNote ecc_balance:", balance)
    print("Nullifier hash:", nullifier_hash)
    print("skcommit:", skcommit)

if __name__ == "__main__":
    main()
