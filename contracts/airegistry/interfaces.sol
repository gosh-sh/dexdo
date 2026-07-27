pragma gosh-solidity >=0.76.1;

interface ISuperRootRegistry {
    function registerRoot(uint256 ownerPubkey) external;
}

interface IRootModelRegistry {
    function registerTokenContract(uint256 sellerPubkey, uint64 nonce) external;
}
