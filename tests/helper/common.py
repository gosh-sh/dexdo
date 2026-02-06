import json
import os
import platform
import re
import shutil
import socket
import subprocess
import sys
import time

from dataclasses import asdict, is_dataclass
from dataclasses import dataclass
from datetime import datetime
from enum import Enum
from pathlib import Path

HALO2_PROOVER_PATH = "./halo2-proover"

COMPILER_DIR = "./contracts/compiler"

SOLD = os.getenv('SOLD', shutil.which("sold") or f"{COMPILER_DIR}/sold")
TVM_CLI = os.getenv('CLI_NAME', shutil.which("tvm-cli") or f"{COMPILER_DIR}/tvm-cli")
TVM_DEBUGGER = os.getenv('TVM_DEBUGGER', shutil.which("tvm-debugger") or f"{COMPILER_DIR}/tvm-debugger")

NETWORK = os.getenv('NETWORK', 'http://127.0.0.1:80')
WORK_DIR = None
GIVER_PATH = "tests/GiverV3"

WAS_ERROR = False
ECC_KEY = 2

RFC1918 = re.compile(
    r"^(10\.)|"
    r"(192\.168\.)|"
    r"(172\.(1[6-9]|2[0-9]|3[0-1])\.)"
)


class SkippedReason(Enum):
    NoState = 0
    BadState = 1
    NoGas = 2
    Suspended = 3


def create_dark_dex_halo2_proof(sk_u: str, token_type: str, private_note_sum: str) -> dict:
    cmd = f"{HALO2_PROOVER_PATH} {sk_u} {token_type} {private_note_sum}"
    output = execute_cmd(cmd, WORK_DIR, True, False)
    try:
        res = json.loads(output)
    except json.JSONDecodeError:
        res = None
        print(f"Failed to decode output as json: '{output}'")

    return res

def __check_cli__():
    print(f"Checking cli specified in library: {TVM_CLI}")
    execute_cmd(f"{TVM_CLI} version")


def setup():
    execute_cli_cmd(f"config clear")
    execute_cli_cmd(f"config --url {NETWORK}")


def set_config(options: dict):
    options_str = ""
    for key in options:
        options_str = f"{options_str} --{key} {options[key]}"
    execute_cli_cmd(f"config {options_str}")

def execute_cmd(command: str, work_dir=None, ignore_error=False, silent=False):
    global WAS_ERROR
    if work_dir is not None:
        command = f"cd {work_dir} && {command}"
 
    WAS_ERROR = False
    if not silent:
        print(command)
    try:
        output = subprocess.check_output(command, shell=True, stderr=subprocess.STDOUT).decode("utf-8")
        print(output)
    except subprocess.CalledProcessError as e:
        output = e.output.decode("utf-8")
        WAS_ERROR = True
        if not ignore_error:
            print(f"Command `{command}` execution failed: {output} {e.stderr}")
            exit(1)

    return output.strip()


def execute_cli_cmd(cmd: str, print_output=False) -> dict:
    cmd = f"{TVM_CLI} -j {cmd}"
    output = execute_cmd(cmd, WORK_DIR, True, False)

    if print_output:
        print(f"Execution result: {output}")
    try:
        res = json.loads(output)
    except json.JSONDecodeError:
        res = None
        print(f"Failed to decode output as json: '{output}'")
    return res

def execute_cmd_without_exit(command: str, work_dir=None, ignore_error=False, silent=False):
    global WAS_ERROR
    if work_dir is not None:
        command = f"cd {work_dir} && {command}"
    WAS_ERROR = False
    if not silent:
        print(command)
    try:
        output = subprocess.check_output(command, shell=True, stderr=subprocess.STDOUT).decode("utf-8")
        print(output)
    except subprocess.CalledProcessError as e:
        output = e.output.decode("utf-8")
        WAS_ERROR = True
        #if not ignore_error:
        print(f"Command `{command}` execution failed: {output} {e.stderr}")

    return output.strip()

def execute_cli_cmd_without_exit(cmd: str) -> dict:
    cmd = f"{TVM_CLI} -j {cmd}"
    output = execute_cmd_without_exit(cmd, WORK_DIR, True, False)
    try:
        res = json.loads(output)
    except json.JSONDecodeError:
        res = None
        print(f"Failed to decode output as json: '{output}'")
    return res

def get_contract_path_stem(input_path: str) -> str:
    if input_path.endswith(".tvc"):
        input_path = input_path.split(".tvc")[0]
    if input_path.endswith(".sol"):
        input_path = input_path.split(".sol")[0]
    if input_path.endswith(".abi"):
        input_path = input_path.split(".abi")[0]
    return input_path


def get_code(tvc_path: str) -> str:
    subcmd = f"decode stateinit --tvc {tvc_path}"
    return execute_cli_cmd(subcmd, True)["code"]


def generate_address(contract_path: str, key_path: str = None) -> str:
    contract_path = get_contract_path_stem(contract_path)
    if key_path is None:
        keys_option = f' --genkey {contract_path}.keys.json'
    else:
        if os.path.isabs(key_path):
            abs_key_path = key_path
        else:
            abs_key_path = f'{WORK_DIR}/{key_path}'
        if os.path.exists(abs_key_path):
            keys_option = f' --setkey {key_path}'
        else:
            keys_option = f' --genkey {key_path}'

    subcmd = f'genaddr --abi {contract_path}.abi.json {keys_option} --save {contract_path}.tvc'
    return execute_cli_cmd(subcmd)["raw_address"]

def generate_address_with_init_data(contract_path: str, init_data: str = None, key_path: str = None) -> str:
    contract_path = get_contract_path_stem(contract_path)
    if key_path is None:
        keys_option = f' --genkey {contract_path}.keys.json'
    else:
        if os.path.isabs(key_path):
            abs_key_path = key_path
        else:
            abs_key_path = f'{WORK_DIR}/{key_path}'
        if os.path.exists(abs_key_path):
            keys_option = f' --setkey {key_path}'
        else:
            keys_option = f' --genkey {key_path}'

    subcmd = f'genaddr --data {init_data} --abi {contract_path}.abi.json {keys_option} --save {contract_path}.tvc'
    return execute_cli_cmd(subcmd)["raw_address"]

def send_from_giver(address: str, value: int):
    with open(f'{GIVER_PATH}.address', 'r') as file:
        giver_address = file.read().rstrip()
    print(f"{giver_address=}")
    subcmd = f'call --abi {GIVER_PATH}.abi.json --keys \
{GIVER_PATH}.keys.json {giver_address} sendCurrencyWithFlag \'{{\"value\":{value},\"bounce\":false,\"dest\":\"{address}\",\"flag\":16,\"ecc\":{{\"2\": {value}}}}}\''
    execute_cli_cmd(subcmd)
    subcmd = f'call --abi {GIVER_PATH}.abi.json --keys \
{GIVER_PATH}.keys.json {giver_address} sendCurrencyWithFlag \'{{\"value\":{value},\"bounce\":false,\"dest\":\"{address}\",\"flag\":2,\"ecc\":{{\"2\": {value}}}}}\''
    execute_cli_cmd(subcmd)

def get_account(address: str, print_output=False) -> dict:
    subcmd = f"account {address}"
    return execute_cli_cmd(subcmd, print_output)

def get_balance(address: str) -> int:
    return get_account(address)["balance"]


def is_account_active(address: str) -> bool:
    account = get_account(address)
    if 'acc_type' not in account:
        return False
    return account["acc_type"] == "Active"

def wait_account_status(address: str, status: str) -> bool:
    start_time = time.time()
    timeout = 10  # seconds

    while time.time() - start_time < timeout:
        account = get_account(address)
        if 'acc_type' in account.keys() and account['acc_type'] == status:
            return True
        time.sleep(.5)

    return False


def wait_account_uninit(address: str):
    result = wait_account_status(address, "Uninit")
    assert result == True


# Sleep with progress bar
def sleep(interval: int):
    print(f"{datetime.now().strftime('%H:%M:%S')} Sleep {interval}s")

    try:
        from tqdm import tqdm
        for _i in tqdm(range(interval)):
            time.sleep(1)
    except ImportError as e:
        time.sleep(interval)



def wait_for_block_seq_no(block_seq_no: int, max_attempts: int = 20):
    while True:
        max_attempts -= 1
        assert max_attempts >= 0
        last_seq_no = get_latest_block_seq_no()
        break_flag = True
        if last_seq_no is not None:
            if last_seq_no < block_seq_no:
                break_flag = False
        else:
            break_flag = False
        if not break_flag:
            sleep(10)
            continue
        break

def wait_account_active(address: str):
    result = wait_account_status(address, "Active")
    assert result == True


def format_params(params: dict) -> str:
    if params is None:
        return ""

    def custom_serializer(obj, seen=None):
        if seen is None:
            seen = set()

        if isinstance(obj, (int, float, bool, str)):
            return obj
        if obj is None:
            return None
        if isinstance(obj, set):
            return list(obj)
        if isinstance(obj, list):
            return [custom_serializer(item, seen) for item in obj]
        if isinstance(obj, dict):
            return {key: custom_serializer(value, seen) for key, value in obj.items()}
        if is_dataclass(obj):
            return {key: custom_serializer(value, seen) for key, value in asdict(obj).items()}

        return obj

    formatted_params = json.dumps(custom_serializer(params), separators=(",", ":"), allow_nan=False).replace("'", "'\"'\"'")
    return f'\'{formatted_params}\''

def deploy_contract(
        contract_path: str,
        value: int = 100_000_000_000,
        key_path: str = None,
        constructor_params: dict = {},
        abi_path: str = None
) -> str:
    contract_path = get_contract_path_stem(contract_path)
    contract_address = generate_address(f"{contract_path}.tvc", key_path)
    send_from_giver(contract_address, value)

    max_attempts = 10
    while True:
        max_attempts -= 1
        assert max_attempts != 0
        account = get_account(contract_address)
        if 'acc_type' in account:
            break
        time.sleep(10)

    if key_path is None:
        key_path = f"{contract_path}.keys.json"
    if abi_path is None:
        abi_path = f"{contract_path}.abi.json"
    execute_cli_cmd(f"deployx --abi {abi_path} --keys {key_path} {contract_path}.tvc {format_params(constructor_params)}")

    check_attempts = 10
    while True:
        account = get_account(contract_address)
        if "acc_type" in account:
            if account["acc_type"] == "Active":
                return contract_address
        check_attempts -= 1
        if check_attempts == 0:
            raise NameError('Failed to deploy contract')
        time.sleep(10)


def call_contract(address: str, abi: str, keys: str|None, method: str, params=None, print_output=False) -> dict:
    if isinstance(params, dict) or params is None:
        params = format_params(params)

    arg_keys = f"--keys {keys}" if keys is not None else ""
    return execute_cli_cmd(
        f"callx --abi {abi} --addr {address} {arg_keys} -m {method} {params}",
        print_output
    )


def run_getter(address: str, abi: str, method: str, params: dict = None) -> dict:
    return execute_cli_cmd(
        f"runx --abi {abi} --addr {address} -m {method} {format_params(params)}"
    )


def tons(value) -> int:
    return int(value) * 1_000_000_000


def gen_keys(path: str):
    execute_cli_cmd(f"genphrase --dump {path}")


def read_public_key(path: str) -> str:
    with open(path, 'r') as file:
        key_pair = file.read().rstrip()
    pair = json.loads(key_pair)
    return pair["public"]

def read_bls_public_key(path: str) -> str:
    with open(path, 'r') as file:
        key_pair = file.read().rstrip()
    pair = json.loads(key_pair)[0]
    return pair["public"]


def generate_bls_key(path):
    bls_key = execute_cmd(f"./target/release/node-helper bls")
    key_pair = json.loads(bls_key)

    with open(path, 'w') as file:
        file.write(json.dumps(key_pair, indent=2))
    return key_pair["public"]


def get_latest_block_seq_no():
    data = execute_cli_cmd('''query-raw blocks seq_no --limit 1 --order \'[{"path":"seq_no","direction":"DESC"}]\'''')
    if data is not None and len(data) > 0:
        return int(data[0]["seq_no"])
    else:
        return None


def get_accounts_by_code_hash(code_hash: str):
    cmd = f'''query-raw accounts "id" --order '[ {{ "path": "last_paid", "direction": "DESC" }} ]' --filter '{{"code_hash":{{"eq":"{code_hash}"}}}}' '''
    data = execute_cli_cmd(cmd)
    accounts = []
    for acc in data:
        accounts.append(acc['id'])
    return accounts


def get_error_extensions(result: dict) -> dict:
    return result.get('Error', {}).get('data', {}).get('node_error', {}).get('extensions', {})


def is_tvm_error(result: dict, err_code: int) -> bool:
    return get_error_extensions(result).get('details', {}).get('exit_code') == err_code


def is_compute_skipped(result: dict, exit_code: SkippedReason = None) -> bool:
    ext = get_error_extensions(result)
    return ext.get('code') == "COMPUTE_SKIPPED" and (exit_code is None or ext.get('details', {}).get('exit_code') == exit_code.value)


def is_ok(result) -> bool:
    return 'Error' not in result.keys()


def is_error(result) -> bool:
    return not is_ok(result)


@dataclass
class Message:
    source: str = ""
    destination: str = ""
    value: int = 0
    state_init: bool = False
    boc: str = ""
    body_hex: str = ""
    serialized_data: str = ""


def get_messages_from_linker_test_log(output: str):
    messages = []
    cur_message = Message()
    first_message = True

    start_message_lookup = 'Action(SendMsg)'
    source_lookup = "   source      "
    dest_lookup = "  destination "
    value_lookup = "   value       "
    init_lookup = "init  : "
    boc_lookup = "boc_base64:"
    body_lookup = "body_base64: "
    serialized_data = "serialized_data: "

    for line in output.split('\n'):
        if start_message_lookup in line:
            if first_message:
                first_message = False
            else:
                messages.append(cur_message)
                cur_message = Message()
        if source_lookup in line:
            cur_message.source = line.split(":")[-1].strip()
        if dest_lookup in line:
            cur_message.destination = line.split(":")[-1].strip()
        if value_lookup in line:
            cur_message.value = line.split(":")[-1].strip()
        if init_lookup in line:
            cur_message.state_init = line.split(":")[-1].strip() != "None"
        if boc_lookup in line:
            cur_message.boc = line.split(":")[-1].strip()
        if body_lookup in line:
            cur_message.body_hex = line.split(":")[-1].strip()
        if serialized_data in line:
            cur_message.serialized_data = line.split(":")[-1].strip()
    messages.append(cur_message)
    return messages


def test_wrapper(func):
    def wrapper(*args, **kwargs):
        print(f"running: {func.__name__}...")
        try:
            result = func(*args, **kwargs)
            print(f"{func.__name__}: ok\n")
            return result
        except Exception as e:
            print(f"error: {e}")
            print(f"{func.__name__}: fail\n")
            sys.exit(1)

    return wrapper


def num2addr(value: int) -> str:
    return f"0:{value:064x}"


def hex2dec(value: str) -> int:
    return int(value, 16) if value.startswith("0x") else int(value)


def get_map_bitmask_thread(filename: str) -> dict[str, str]:
    pattern = re.compile(
        r"Bitmask\s*\{\s*mask_bits:\s*([0-9a-fA-F]+).*?ThreadIdentifier<([0-9a-fA-F]+)>",
        re.DOTALL,
    )

    result = {}

    with open(filename, "r", encoding="utf-8") as f:
        content = f.read()

        matches = pattern.findall(content)
        for mask_bits, thread_id in matches:
            result[mask_bits] = thread_id

    return result

def remove_files(path: str):
    p = Path(path)

    if not p.exists():
        print(f"Path does not exist: {p}")
        return
    if os.getenv("CMD_PREFIX") is not None:
        assert p != Path("/")
        execute_cmd(f"sudo rm -rf {p} | true")
        print(f"Removed path with sudo: {p}")
        return
    if p.is_file() or p.is_symlink():
        p.unlink()
        print(f"Removed file: {p}")
    elif p.is_dir():
        shutil.rmtree(p)
        print(f"Removed directory (recursively): {p}")
    else:
        print(f"Unknown file type: {p}")


def get_default_gateway_ip():
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("1.1.1.1", 80))  # Cloudflare DNS
        ip = s.getsockname()[0]
        s.close()
        return ip
    except Exception as e:
        return f"Error: {e}"


def get_lan_ipv4() -> str:
    system = platform.system()

    if system == "Windows":
        return _lan_ipv4_windows()
    elif system == "Darwin":
        return _lan_ipv4_macos()
    elif system == "Linux":
        return _lan_ipv4_linux()
    else:
        raise RuntimeError(f"Unsupported OS: {system}")


def _lan_ipv4_windows() -> str:
    out = subprocess.check_output(
        ["ipconfig"], text=True, encoding="utf-8", errors="ignore"
    )

    for line in out.splitlines():
        if "IPv4 Address" in line or "IPv4-адрес" in line:
            ip = line.split(":")[-1].strip()
            if RFC1918.match(ip):
                return ip

    raise RuntimeError("LAN IPv4 not found on Windows")


def _lan_ipv4_macos() -> str:
    out = subprocess.check_output(["ifconfig"], text=True)

    for line in out.splitlines():
        line = line.strip()
        if not line.startswith("inet "):
            continue
        if "broadcast" not in line:
            continue
        if "-->" in line:
            continue

        ip = line.split()[1]
        if RFC1918.match(ip):
            return ip

    raise RuntimeError("LAN IPv4 not found on macOS")


def _lan_ipv4_linux() -> str:
    out = subprocess.check_output(
        ["ip", "-4", "-o", "addr", "show"], text=True
    )

    for line in out.splitlines():
        parts = line.split()
        ip = parts[3].split("/")[0]
        if RFC1918.match(ip):
            return ip

    raise RuntimeError("LAN IPv4 not found on Linux")


__check_cli__()