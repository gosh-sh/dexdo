import os
import pathlib
import sys
from datetime import datetime

sys.path.append("tests")
from helper import common


ACKI_NACKI_SERVERS = [
    "n11.validators.gosh.sh",
    "n12.validators.gosh.sh",
    "n13.validators.gosh.sh",
    "n14.validators.gosh.sh",
    "n15.validators.gosh.sh"
]

REMOTE_USER_NAME = "gosh"
KEY_PATH = "~/old_ssh/.ssh/id_rsa"
PORT = 22488

OUTPUT_PATH_TEMPLATE = "./test_servers/logs/{date}/node_{node_id}_{time}"


def query_log_path(node_id: int) -> str:
    cmd_str = f"ssh -i {KEY_PATH} {REMOTE_USER_NAME}@{ACKI_NACKI_SERVERS[node_id]} -p {PORT} 'docker inspect \
ackinacki-{node_id}-node-1 | jq -r .\"[0]\".LogPath'"
    return common.execute_cmd(cmd_str)


def generate_output_path(node_id: int) -> str:
    now = datetime.now()
    date = now.strftime('%Y_%m_%d')
    time = now.strftime('%H_%M_%S')
    output_path = OUTPUT_PATH_TEMPLATE.format(date=date, node_id=node_id, time=time)
    pathlib.Path(os.path.dirname(output_path)).mkdir(parents=True, exist_ok=True)
    return output_path


def rsync_file(node_id: int):
    log_path = query_log_path(node_id)
    print(f"{log_path=}")
    output_path = generate_output_path(node_id)
    cmd_str = f"rsync -avzh -e \"ssh -i {KEY_PATH} -p {PORT}\" --rsync-path=\"sudo rsync\" \
{REMOTE_USER_NAME}@{ACKI_NACKI_SERVERS[node_id]}:{log_path} {output_path}"
    common.execute_cmd(cmd_str)


def get_logs_for_all_nodes():
    for i in range(0, len(ACKI_NACKI_SERVERS)):
        rsync_file(i)


if __name__ == "__main__":
    get_logs_for_all_nodes()
