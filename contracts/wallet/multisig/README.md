**Multisig** – A multisignature wallet with support for custodian update and SHELL exchange features.

Derived from `UpdateCustodianMultisigWallet`. The message-sending entry points
(`sendTransaction`, `submitTransaction`) additionally accept a `dapp_id`: it is
provided by the caller and stored in the queued `Transaction`, but it is **not**
used when the outbound message is actually sent (kept for off-chain/API purposes).

### Building `Multisig` with `sold`

`sold` is an all-in-one compiler and linker for the TVM Solidity language, available as a single binary.

To manually build `sold`, follow [this guide](https://github.com/gosh-sh/TVM-Solidity-Compiler?tab=readme-ov-file#build-and-install), or download the binaries directly from [here](https://github.com/gosh-sh/TVM-Solidity-Compiler/releases).

To compile the `Multisig` contract:

```bash
sold --tvm-version gosh  Multisig.sol
```
