import re
import sys
import argparse

sys.path.append("tests")

logs_dir = "./logs"


def find_last_trace_time(node_id: int):
    # Regular expression to match the log lines with the desired format
    end_pattern = re.compile(
        r"(\d{4}-\d{2}-\d{2})T(\d{2}:\d{2}:\d{2})\.\d+Z TRACE .*End block production process iteration.*")
    seq_no_pattern = re.compile(r".*seq_no:\s*(\d+)")

    last_end_time = ""
    last_seq_no = ""

    # Open the log file and iterate over each line
    with open(f"{logs_dir}/node{node_id}/node.log", "r") as file:
        for line in file:
            if "End block production process iteration" in line:
                match_end = end_pattern.match(line)
                if match_end:
                    last_end_time = match_end.group(2)
            if 'Last finalized block data: seq_no:' in line:
                match_seq_no = seq_no_pattern.match(line)
                if match_seq_no:
                    last_seq_no = match_seq_no.group(1)

    if last_end_time == "":
        print(f"node{node_id}: {last_seq_no}")
    else:
        print(f"node{node_id}: {last_seq_no}, produce block time: {last_end_time}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Node log analyser")
    parser.add_argument(
        "-p", "--path",
        nargs="?",
        default="./logs",
        help="path to node logs"
    )
    args = parser.parse_args()
    logs_dir = args.path
    for i in range(0, 5):
        find_last_trace_time(i)
