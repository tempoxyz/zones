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

        /// Active sequencer encryption key (compressed secp256k1 point).
        function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);

        /// Total number of encryption keys ever registered.
        function encryptionKeyCount() external view returns (uint256);
    }
}

sol! {
    /// TIP-20 token mint function.
    function mint(address to, uint256 amount);
}

impl<P: alloy_provider::Provider<N>, N: alloy_network::Network>
    ZonePortal::ZonePortalInstance<P, N>
{
    /// Returns all token addresses currently enabled for bridging on this [`ZonePortal`].
    ///
    /// Calls [`enabledTokenCount`](ZonePortal::enabledTokenCountCall) followed by
    /// [`enabledTokenAt`](ZonePortal::enabledTokenAtCall) for each index.
    pub async fn enabled_tokens(
        &self,
    ) -> Result<Vec<alloy_primitives::Address>, alloy_contract::Error> {
        let count = self.enabledTokenCount().call().await?;
        let mut tokens = Vec::with_capacity(count.to::<usize>());
        for i in 0..count.to::<u64>() {
            let token = self
                .enabledTokenAt(alloy_primitives::U256::from(i))
                .call()
                .await?;
            tokens.push(token);
        }
        Ok(tokens)
    }

    /// Fetches the active sequencer encryption key and its index.
    ///
    /// Returns `(key, key_index)` where `key` is the
    /// [`sequencerEncryptionKeyReturn`](ZonePortal::sequencerEncryptionKeyReturn) and
    /// `key_index` is the zero-based index of the current key.
    pub async fn encryption_key(
        &self,
    ) -> Result<
        (ZonePortal::sequencerEncryptionKeyReturn, alloy_primitives::U256),
        alloy_contract::Error,
    > {
        let key_call = self.sequencerEncryptionKey();
        let count_call = self.encryptionKeyCount();
        let (key, count) = tokio::try_join!(key_call.call(), count_call.call())?;
        let key_index = count.saturating_sub(alloy_primitives::U256::from(1));
        Ok((key, key_index))
    }
}
