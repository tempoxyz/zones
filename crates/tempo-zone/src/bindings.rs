//! Solidity contract bindings for the Zone.

use alloy_sol_types::sol;

sol! {
    /// ZonePortal contract on L1.
    #[sol(rpc)]
    contract ZonePortal {
        /// Event emitted when a deposit is made.
        #[derive(Debug)]
        event DepositMade(
            bytes32 indexed newCurrentDepositQueueHash,
            address indexed sender,
            address token,
            address to,
            uint128 netAmount,
            uint128 fee,
            bytes32 memo
        );

        /// Event emitted when an encrypted deposit is made.
        #[derive(Debug)]
        event EncryptedDepositMade(
            bytes32 indexed newCurrentDepositQueueHash,
            address indexed sender,
            address token,
            uint128 netAmount,
            uint128 fee,
            uint256 keyIndex,
            bytes32 ephemeralPubkeyX,
            uint8 ephemeralPubkeyYParity,
            bytes ciphertext,
            bytes12 nonce,
            bytes16 tag
        );

        /// Last synced Tempo block number.
        function lastSyncedTempoBlockNumber() external view returns (uint64);

        /// Genesis Tempo block number.
        function genesisTempoBlockNumber() external view returns (uint64);
    }
}

sol! {
    /// TIP-20 token mint function.
    function mint(address to, uint256 amount);
}
