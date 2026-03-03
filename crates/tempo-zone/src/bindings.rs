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

        /// Event emitted when a new TIP-20 token is enabled for bridging.
        /// Includes token metadata so the zone can create a matching TIP-20.
        #[derive(Debug)]
        event TokenEnabled(address indexed token, string name, string symbol, string currency);

        /// Last synced Tempo block number.
        function lastSyncedTempoBlockNumber() external view returns (uint64);

        /// Genesis Tempo block number.
        function genesisTempoBlockNumber() external view returns (uint64);

        /// Number of enabled tokens.
        function enabledTokenCount() external view returns (uint256);

        /// Enabled token by index.
        function enabledTokenAt(uint256 index) external view returns (address);
    }
}

sol! {
    /// TIP-20 token mint function.
    function mint(address to, uint256 amount);
}

/// Query all enabled tokens from a ZonePortal contract.
pub async fn enabled_tokens(
    portal_address: alloy_primitives::Address,
    provider: &impl alloy_provider::Provider<tempo_alloy::TempoNetwork>,
) -> eyre::Result<Vec<alloy_primitives::Address>> {
    let portal = ZonePortal::new(portal_address, provider);
    let count: u64 = portal.enabledTokenCount().call().await?.try_into()?;
    let mut tokens = Vec::with_capacity(count as usize);
    for i in 0..count {
        let token = portal
            .enabledTokenAt(alloy_primitives::U256::from(i))
            .call()
            .await?;
        tokens.push(token);
    }
    Ok(tokens)
}
