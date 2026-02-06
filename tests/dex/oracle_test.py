import sys
import time
import json

sys.path.append("tests")
from helper import common

ROOT_ORACLE_ADDRESS = "0:1515151515151515151515151515151515151515151515151515151515151515"
ROOT_ORACLE_ABI = "./contracts/dex/RootOracle.abi.json"
EPHEMERAL_KEY_PATH = "./tests/dex/ephemeral.keys.json"
ORACLE_KEY_PATH = "./tests/dex/oracle.keys.json"

ORACLE_ABI = "./contracts/dex/Oracle.abi.json"
EVENTLIST_ABI = "./contracts/dex/OracleEventList.abi.json"

ORACLE_NAME = "MyOracle"
EVENT_NAME = "Winner of match X"
ORACLE_FEE = 100
DEADLINE = 2000000000
DESCRIBE = "Who will win match X"
OUTCOMES = {1: "Team A", 2: "Team B"}

ECC_KEY = "1"
VAULT_DEPOSIT = 1000
GIVER_ADDRESS = "0:1111111111111111111111111111111111111111111111111111111111111111"
GIVER_ABI = "./contracts/giver/GiverV3.abi.json"
GIVER_KEY_PATH = "./tests/GiverV3.keys.json"


def generate_oracle_pubkey():
    common.gen_keys(ORACLE_KEY_PATH)
    pubkey_str = common.read_public_key(ORACLE_KEY_PATH)

    if not pubkey_str.startswith("0x"):
        pubkey_str = "0x" + pubkey_str

    print(f"Generated Oracle pubkey: {pubkey_str}")
    return pubkey_str


def deploy_oracle(oracle_pubkey):
    params = {"oraclePubkey": oracle_pubkey, "oracleName": ORACLE_NAME}
    print(f"Deploying Oracle with params: {params}")

    result = common.call_contract(
        ROOT_ORACLE_ADDRESS,
        ROOT_ORACLE_ABI,
        EPHEMERAL_KEY_PATH,
        "deployOracle",
        params
    )
    time.sleep(1)

    print(f"Deploy Oracle result:\n{result}")


def get_oracle_address():
    params = {"name": ORACLE_NAME}

    result = common.run_getter(
        ROOT_ORACLE_ADDRESS,
        ROOT_ORACLE_ABI,
        "getOracleAddress",
        params
    )
    time.sleep(1)

    oracle_address = result.get("oracleAddress") if isinstance(result, dict) else result
    print(f"Oracle address: {oracle_address}")

    common.wait_account_active(oracle_address)
    time.sleep(1)

    active = common.is_account_active(oracle_address)
    time.sleep(1)

    assert active is True
    return oracle_address


def get_eventlist_address(oracle_address, index=0):
    params = {"index": index}

    result = common.run_getter(
        oracle_address,
        ORACLE_ABI,
        "getEventListAddress",
        params
    )
    time.sleep(1)

    eventlist_address = result.get("value0") if isinstance(result, dict) else result
    print(f"EventList address: {eventlist_address}")

    common.wait_account_active(eventlist_address)
    time.sleep(1)

    active = common.is_account_active(eventlist_address)
    time.sleep(1)

    assert active is True
    return eventlist_address

def add_event(eventlist_address):
    params = {
        "event_name": EVENT_NAME,
        "oracle_fee": ORACLE_FEE,
        "deadline": DEADLINE,
        "describe": DESCRIBE,
        "outcomeNames": OUTCOMES
    }

    print(f"Adding event '{EVENT_NAME}' to EventList {eventlist_address}")

    result = common.call_contract(
        eventlist_address,
        EVENTLIST_ABI,
        ORACLE_KEY_PATH,
        "addEvent",
        params
    )
    time.sleep(1)

    print(f"Add event result:\n{result}")

    account_data = common.run_getter(
        eventlist_address,
        EVENTLIST_ABI,
        "_events",
        {"": 0}
    )
    time.sleep(1)

    assert account_data
    print("EventList has events: True")


def main():
    oracle_pubkey = generate_oracle_pubkey()
    deploy_oracle(oracle_pubkey)

    oracle_address = get_oracle_address()
    eventlist_address = get_eventlist_address(oracle_address)

    add_event(eventlist_address)

    print("Oracle and EventList deployment completed successfully")


if __name__ == "__main__":
    main()
