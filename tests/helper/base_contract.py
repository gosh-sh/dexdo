"""
2022-2025 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
"""

# import json
import asyncio
from concurrent.futures import ThreadPoolExecutor

from .common import execute_cli_cmd, format_params

executor = ThreadPoolExecutor(max_workers=5)


class BaseContract:
    """The :class:`BaseContract <BaseContract>` object, which is responsible
    for interaction with deployed contracts.
    """

    def __init__(
        self, name: str, abi_path: str = None, address: str = None, nickname: str = None
    ):
        """Constructs :class:`BaseContract <BaseContract>` object.

        :param str name: Name used to load contract's bytecode and ABI
        :param str address: If this parameter is specified no new contract is created
            but instead a wrapper for an existing contract is created
        :param str nickname: Nickname of the contract used in verbose output
        """
        self.name_ = name
        self.addr_ = address
        self.nickname_ = nickname
        # with open(abi_path, "rb") as fp:
        #     data = json.load(fp)
        # self.abi_ = dict(path_=abi_path, json=data)
        self.abi_ = abi_path

    @property
    def address(self) -> str:
        """Returns address of a given contract.

        :return: Address of contract
        :rtype: str
        """
        return self.addr_

    @property
    def abi(self) -> str:
        """Returns ABI of a given kind of contract.

        :return: ABI of contract
        :rtype: type
        """
        return self.abi_

    def run(self, method: str, params=None) -> dict:
        """Calls a given getter and decodes an answer.

        :param str method: Name of a getter
        :param dict params: A dictionary with getter parameters
        :return: A returned value in decoded form (exact type depends on the type of getter)
        :rtype: type
        """
        if params is None:
            params = {}

        return execute_cli_cmd(
            f"runx --abi {self.abi} --addr {self.address} -m {method} {format_params(params)}"
        )

    async def run_async(self, method: str, params=None) -> dict:
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(executor, self.run, method, params)

    def call(self, method: str, params=None, thread: str = None) -> dict:
        """Calls a given method.

        :param str method: Name of the method to be called
        :param dict params: A dictionary with parameters for calling the contract function
        :return: Value in decoded form (if method returns something)
        :rtype: type
        """
        if params is None:
            params = {}

        thread_arg = f"--thread {thread}" if thread is not None else ""
        return execute_cli_cmd(
            f"callx --abi {self.abi} --addr {self.address} {thread_arg} -m {method} {params}"
        )

    async def call_async(self, method: str, params=None, thread=None) -> dict:
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(executor, self.call, method, params, thread)
