pragma gosh-solidity >=0.76.1;
pragma AbiHeader expire;
pragma AbiHeader pubkey;

import "./modifiers/modifiers.sol";
import "./RootPN.sol";

/// @title Nullifier Contract
contract Nullifier is Modifiers {
    /// @notice Contract semantic version.
    string constant version = "1.0.2";

    /// @notice Nullifier hash used as deterministic deployment key.
    uint256 static _nullifier_hash;

    /// @notice Nullifier constructor.
    /// @param to Recipient PrivateNote address that receives transferred shell funds.
    constructor(address to) {
        tvm.accept();
        require(msg.sender == ROOT_PN_ADDRESS, ERR_INVALID_SENDER);
        mapping(uint32 => varuint32) data_cur;
        data_cur[CURRENCIES_ID_SHELL] = msg.currencies[CURRENCIES_ID_SHELL];
        to.transfer({value: 0.1 vmshell, bounce: false, currencies: data_cur});
    }

    /// @notice Returns contract version
    /// @return value0 Contract semantic version.
    /// @return value1 Contract identifier.
    function getVersion() external pure returns (string, string) {
        return (version, "Nullifier");
    }
}
