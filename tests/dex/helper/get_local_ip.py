import contextlib
import io
import importlib
import os
import sys

sys.path.append("tests")

# suppress output from __check_cli__()
with open(os.devnull, "w") as buf, contextlib.redirect_stdout(buf), contextlib.redirect_stderr(buf):
    common = importlib.import_module("helper.common")


if __name__ == "__main__":
    print(common.get_lan_ipv4())
