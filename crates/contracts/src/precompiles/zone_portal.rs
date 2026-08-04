//! `ZonePortal` — deployed on Tempo L1.

pub use ZonePortal::{
    BlockTransition, DepositQueueTransition, EncryptedDeposit, EncryptedDepositPayload, Withdrawal,
    ZonePortalErrors as ZonePortalError,
};

use crate::{IZoneOutbox, ZoneInboxEvent};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_sol_types::SolValue;
use zone_primitives::constants::EMPTY_SENTINEL;

crate::sol! {
    #[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
    contract ZonePortal {
        // -- Shared types --
        enum Role {
            None,
            Account,
            CallbackGateway
        }

        struct Withdrawal {
            address token;
            bytes32 senderTag;
            address to;
            uint128 amount;
            bytes32 memo;
            uint64 gasLimit;
            uint64 fallbackNonce;
            bytes callbackData;
            bytes encryptedSender;
        }

        /// Encrypted deposit payload (ECIES encrypted recipient and memo)
        struct EncryptedDepositPayload {
            bytes32 ephemeralPubkeyX;
            uint8 ephemeralPubkeyYParity;
            bytes ciphertext;
            bytes12 nonce;
            bytes16 tag;
        }

        /// Encrypted deposit stored in the queue
        struct EncryptedDeposit {
            address token;
            address sender;
            uint128 amount;
            address tempoRefundRecipient;
            uint256 keyIndex;
            EncryptedDepositPayload encrypted;
        }

        struct EncryptionKeyEntry {
            bytes32 x;
            uint8 yParity;
            uint64 activationBlock;
        }

        struct BlockTransition {
            bytes32 prevBlockHash;
            bytes32 nextBlockHash;
        }

        struct DepositQueueTransition {
            bytes32 prevProcessedHash;
            bytes32 nextProcessedHash;
            uint64 prevDepositNumber;
            uint64 nextDepositNumber;
        }

        // -- Events --

        event DepositMade(
            bytes32 indexed newCurrentDepositQueueHash,
            address indexed sender,
            address token,
            address to,
            uint128 netAmount,
            uint128 fee,
            bytes32 memo,
            address tempoRefundRecipient,
            uint64 depositNumber
        );

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
            bytes16 tag,
            address tempoRefundRecipient,
            uint64 depositNumber
        );

        /// Event emitted when a new TIP-20 token is enabled for bridging.
        /// Includes token metadata so the zone can create a matching TIP-20.
        event TokenEnabled(address indexed token, string name, string symbol, string currency);

        event SequencerEncryptionKeyUpdated(
            bytes32 x,
            uint8 yParity,
            uint256 keyIndex,
            uint64 activationBlock
        );

        /// `withdrawalQueueIndex` is the logical withdrawal queue index the batch's hash
        /// chain was enqueued under, or `NO_QUEUE_INDEX` when the batch
        /// carried no withdrawals.
        event BatchSubmitted(
            uint64 indexed withdrawalBatchIndex,
            uint256 indexed withdrawalQueueIndex,
            bytes32 nextProcessedDepositQueueHash,
            bytes32 nextBlockHash,
            bytes32 withdrawalQueueHash,
            uint64 lastProcessedDepositNumber
        );

        event WithdrawalProcessed(
            address indexed to,
            bytes32 indexed senderTag,
            address token,
            uint128 amount,
            bool callbackSuccess
        );

        event WithdrawalBounceBack(
            bytes32 indexed newCurrentDepositQueueHash,
            uint64 indexed fallbackNonce,
            address token,
            uint128 amount,
            uint64 depositNumber
        );

        event DepositBounceBack(
            address indexed tempoRefundRecipient,
            address token,
            uint128 amount,
            uint128 bouncebackFee
        );

        event DepositBounceBackPending(
            address indexed tempoRefundRecipient,
            address token,
            uint128 amount,
            uint128 bouncebackFee
        );

        event RefundClaimed(address indexed recipient, address indexed token, uint128 amount);

        event ZoneGasRateUpdated(uint128 zoneGasRate);
        event MaxTempoGasRateUpdated(uint128 maxTempoGasRate);
        event BouncebackGasUpdated(uint64 bouncebackGas);

        event AdminTransferStarted(
            address indexed currentAdmin,
            address indexed pendingAdmin
        );

        event AdminTransferred(
            address indexed previousAdmin,
            address indexed newAdmin
        );

        event RoleUpdated(address indexed account, Role prev, Role next);
        event EnforcementModesUpdated(bool accessMode, bool gatewayMode);
        event SequencerSetUpdated(uint64 indexed nonce, uint8 threshold, address[] sequencers);

        /// Emitted when block-production leadership transitions to a new sequencer.
        /// Zone nodes derive leadership exclusively from finalized observations of this event.
        event LeaderUpdated(
            address indexed previousLeader,
            address indexed newLeader,
            uint64 indexed epoch,
            uint64 activationTempoBlock
        );

        // -- Errors --

        error NotSequencer();
        error NotAdmin();
        error NotPendingAdmin();
        error InvalidProof();
        error InvalidTempoBlockNumber();
        error PolicyForbids();
        error InvalidBouncebackRecipient();
        error TokenNotEnabled();
        error DepositBlockCapacityExceeded(uint64 maximum);
        error InvalidCallbackTarget();
        error AccountNotAllowed(address account);
        error InvalidLeader();
        error ActiveLeaderRemoved();
        error LeaderAlreadyUpdatedThisBlock();
        error StaleLeadershipEpoch(uint64 expected, uint64 actual);

        // -- View functions --

        function zoneId() external view returns (uint32);
        function admin() external view returns (address);
        function messenger() external view returns (address);
        function isAccessEnforced() external view returns (bool);
        function setAccessMode(bool enforced) external;
        function isGatewayOpen() external view returns (bool);
        function setGatewayMode(bool enforced) external;
        function role(address account) external view returns (Role);
        function setRole(address account, Role role) external;
        function setAllowedAccount(address account, bool allowed) external;
        function setGateway(address account, bool allowed) external;
        function setSequencerSet(address[] calldata newSequencers, uint8 newThreshold) external;
        function verifier() external view returns (address);
        function sequencerSetVersion() external view returns (uint64);
        function sequencerThreshold() external view returns (uint8);
        function zoneHeight() external view returns (uint256);
        function isSequencer(address account) external view returns (bool);
        function sequencerCount() external view returns (uint256);
        function sequencerAt(uint256 index) external view returns (address);
        function leader() external view returns (address);
        function leaderEpoch() external view returns (uint64);
        function leaderActivationTempoBlock() external view returns (uint64);
        function setLeader(address newLeader, uint64 expectedEpoch) external;
        function withdrawalBatchIndex() external view returns (uint64);
        function blockHash() external view returns (bytes32);
        function currentDepositQueueHash() external view returns (bytes32);
        function lastSyncedTempoBlockNumber() external view returns (uint64);
        function withdrawalQueueHead() external view returns (uint256);
        function withdrawalQueueTail() external view returns (uint256);
        function withdrawalQueueSlot(uint256 physicalSlot) external view returns (bytes32);
        function calculateDepositFee() external view returns (uint128 fee);
        function calculateBouncebackFee() external view returns (uint128 fee);
        function depositCount() external view returns (uint64);
        function lastProcessedDepositNumber() external view returns (uint64);
        function MAX_DEPOSITS_PER_TEMPO_BLOCK() external view returns (uint64);
        function MAX_WITHDRAWAL_GAS_LIMIT() external view returns (uint64);

        // -- State-changing functions --

        function deposit(
            address token,
            address to,
            uint128 amount,
            bytes32 memo,
            address tempoRefundRecipient
        )
            external
            returns (bytes32 newCurrentDepositQueueHash);

        function processWithdrawals(Withdrawal[] calldata withdrawals, bytes32 remainingQueue) external;

        function submitBatch(
            uint64 tempoBlockNumber,
            uint64 recentTempoBlockNumber,
            BlockTransition calldata blockTransition,
            DepositQueueTransition calldata depositQueueTransition,
            bytes32 withdrawalQueueHash,
            bytes calldata verifierConfig,
            bytes calldata proof,
            uint256 nextZoneHeight,
            bytes[] calldata signatures
        ) external;

        function enableToken(address token) external;
        function pauseDeposits(address token) external;
        function resumeDeposits(address token) external;

        function setZoneGasRate(uint128 newZoneGasRate) external;
        function setMaxTempoGasRate(uint128 newMaxTempoGasRate) external;
        function setBouncebackGas(uint64 newBouncebackGas) external;

        function transferAdmin(address newAdmin) external;
        function acceptAdmin() external;

        function rpcUrl() external view returns (string memory);
        function setRpcUrl(string calldata rpcUrl) external;

        function depositEncrypted(
            address token,
            uint128 amount,
            uint256 keyIndex,
            EncryptedDepositPayload calldata encrypted,
            address tempoRefundRecipient
        ) external returns (bytes32 newCurrentDepositQueueHash);

        function setSequencerEncryptionKey(
            bytes32 x,
            uint8 yParity,
            uint8 popV,
            bytes32 popR,
            bytes32 popS
        ) external;

        // -- View functions (token management) --

        function isTokenEnabled(address token) external view returns (bool);
        function enabledTokenCount() external view returns (uint256);
        function enabledTokenAt(uint256 index) external view returns (address);
        function zoneGasRate() external view returns (uint128);
        function maxTempoGasRate() external view returns (uint128);
        function bouncebackGas() external view returns (uint64);
        function pendingAdmin() external view returns (address);
        function refunds(address token, address owner) external view returns (uint128);

        function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);

        function encryptionKeyCount() external view returns (uint256);
        function encryptionKeyAt(uint256 index)
            external view returns (EncryptionKeyEntry memory entry);
        function isEncryptionKeyValid(uint256 keyIndex)
            external view returns (bool valid, uint64 expiresAtBlock);
        function encryptionKeyAtBlock(uint64 tempoBlockNumber)
            external view returns (bytes32 x, uint8 yParity, uint256 keyIndex);
        function claimRefund(address token) external returns (uint128 amount);
    }
}

#[cfg(feature = "rpc")]
impl<P: alloy_provider::Provider<N>, N: alloy_network::Network>
    ZonePortal::ZonePortalInstance<P, N>
{
    /// Returns all token addresses currently enabled for bridging on this [`ZonePortal`].
    ///
    /// Calls [`enabledTokenCount`](ZonePortal::enabledTokenCountCall) followed by
    /// [`enabledTokenAt`](ZonePortal::enabledTokenAtCall) for each index concurrently.
    pub async fn enabled_tokens(
        &self,
    ) -> Result<alloc::vec::Vec<alloy_primitives::Address>, alloy_contract::Error> {
        self.enabled_tokens_at(alloy_rpc_types_eth::BlockId::latest())
            .await
    }

    /// Returns all token addresses enabled for bridging at `block_id`.
    ///
    /// Callers that pair the returned token list with other historical L1 reads
    /// should use this instead of [`enabled_tokens`](Self::enabled_tokens), so
    /// future `TokenEnabled` events are not mixed into older state snapshots.
    pub async fn enabled_tokens_at(
        &self,
        block_id: alloy_rpc_types_eth::BlockId,
    ) -> Result<alloc::vec::Vec<alloy_primitives::Address>, alloy_contract::Error> {
        let count = self.enabledTokenCount().block(block_id).call().await?;
        let futs: alloc::vec::Vec<_> = (0..count.to::<u64>())
            .map(|i| async move {
                self.enabledTokenAt(alloy_primitives::U256::from(i))
                    .block(block_id)
                    .call()
                    .await
            })
            .collect();
        futures::future::try_join_all(futs).await
    }

    /// Fetches the active sequencer encryption key and its index from one L1 snapshot.
    ///
    /// Reads the current L1 block number, then pins an atomic
    /// [`encryptionKeyAtBlock`](ZonePortal::encryptionKeyAtBlockCall) call to that block so a key
    /// rotation cannot pair a key with an index from a different state snapshot.
    /// Returns `(key, key_index)` where `key` is the
    /// [`sequencerEncryptionKeyReturn`](ZonePortal::sequencerEncryptionKeyReturn) and
    /// `key_index` is the zero-based index of the current key.
    pub async fn encryption_key(
        &self,
    ) -> Result<
        (
            ZonePortal::sequencerEncryptionKeyReturn,
            alloy_primitives::U256,
        ),
        alloy_contract::Error,
    > {
        let block_number = self.provider().get_block_number().await?;
        let key = self
            .encryptionKeyAtBlock(block_number)
            .block(alloy_rpc_types_eth::BlockId::number(block_number))
            .call()
            .await?;
        Ok((
            ZonePortal::sequencerEncryptionKeyReturn {
                x: key.x,
                yParity: key.yParity,
            },
            key.keyIndex,
        ))
    }
}

#[cfg(all(test, feature = "rpc"))]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use alloy_provider::ProviderBuilder;
    use alloy_sol_types::SolCall;
    use alloy_transport::mock::Asserter;

    #[tokio::test]
    async fn encryption_key_reads_key_and_index_from_one_snapshot() {
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let block_number = 42_u64;
        let expected = ZonePortal::encryptionKeyAtBlockReturn {
            x: B256::repeat_byte(0x11),
            yParity: 1,
            keyIndex: U256::from(7),
        };

        asserter.push_success(&block_number);
        asserter.push_success(&Bytes::from(
            ZonePortal::encryptionKeyAtBlockCall::abi_encode_returns(&expected),
        ));

        let portal = ZonePortal::new(Address::ZERO, &provider);
        let (key, key_index) = portal.encryption_key().await.unwrap();

        assert_eq!(key.x, expected.x);
        assert_eq!(key.yParity, expected.yParity);
        assert_eq!(key_index, expected.keyIndex);
        assert!(asserter.read_q().is_empty());
    }
}

impl ZonePortal::sequencerEncryptionKeyReturn {
    /// Normalize `yParity` to SEC1 compressed prefix (`0x02` or `0x03`).
    ///
    /// The contract may return `0`/`1` (parity bit) or `0x02`/`0x03` (SEC1 prefix).
    pub fn normalized_y_parity(&self) -> Option<u8> {
        match self.yParity {
            0x02 | 0x03 => Some(self.yParity),
            0 | 1 => Some(0x02 + self.yParity),
            _ => None,
        }
    }
}

impl core::fmt::Display for ZonePortal::ZonePortalErrors {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotSequencer(_) => f.write_str("NotSequencer"),
            Self::NotAdmin(_) => f.write_str("NotAdmin"),
            Self::NotPendingAdmin(_) => f.write_str("NotPendingAdmin"),
            Self::InvalidProof(_) => f.write_str("InvalidProof"),
            Self::InvalidTempoBlockNumber(_) => f.write_str("InvalidTempoBlockNumber"),
            Self::PolicyForbids(_) => f.write_str("PolicyForbids"),
            Self::InvalidBouncebackRecipient(_) => f.write_str("InvalidBouncebackRecipient"),
            Self::TokenNotEnabled(_) => f.write_str("TokenNotEnabled"),
            Self::DepositBlockCapacityExceeded(_) => f.write_str("DepositBlockCapacityExceeded"),
            Self::InvalidCallbackTarget(_) => f.write_str("InvalidCallbackTarget"),
            Self::AccountNotAllowed(_) => f.write_str("AccountNotAllowed"),
            Self::InvalidLeader(_) => f.write_str("InvalidLeader"),
            Self::ActiveLeaderRemoved(_) => f.write_str("ActiveLeaderRemoved"),
            Self::LeaderAlreadyUpdatedThisBlock(_) => f.write_str("LeaderAlreadyUpdatedThisBlock"),
            Self::StaleLeadershipEpoch(_) => f.write_str("StaleLeadershipEpoch"),
        }
    }
}

impl EncryptedDeposit {
    /// Build the event emitted after a successful encrypted deposit.
    pub fn processed_event(
        &self,
        deposit_hash: B256,
        recipient: Address,
        memo: B256,
    ) -> ZoneInboxEvent {
        ZoneInboxEvent::encrypted_deposit_processed(
            deposit_hash,
            self.sender,
            recipient,
            self.token,
            self.amount,
            memo,
        )
    }

    /// Build the event emitted after a failed encrypted deposit.
    pub fn failed_event(&self, deposit_hash: B256) -> ZoneInboxEvent {
        ZoneInboxEvent::encrypted_deposit_failed(deposit_hash, self.sender, self.token, self.amount)
    }
}

impl Withdrawal {
    /// Build the authenticated-withdrawal sender plaintext `[sender(20) | tx_hash(32)]`.
    pub fn authenticated_sender_plaintext(sender: Address, tx_hash: B256) -> [u8; 52] {
        let mut plaintext = [0u8; 52];
        plaintext[..20].copy_from_slice(sender.as_slice());
        plaintext[20..].copy_from_slice(tx_hash.as_slice());
        plaintext
    }

    /// Compute the authenticated sender tag `keccak256(sender || tx_hash)`.
    pub fn sender_tag(sender: Address, tx_hash: B256) -> B256 {
        keccak256(Self::authenticated_sender_plaintext(sender, tx_hash))
    }

    /// Reconstruct the public L1-facing withdrawal from a zone-side withdrawal request event.
    pub fn from_requested_event(
        event: &IZoneOutbox::WithdrawalRequested,
        tx_hash: B256,
        encrypted_sender: Bytes,
    ) -> Self {
        let sender_tag = if event.sender.is_zero() && event.fallbackNonce == 0 {
            Self::sender_tag(Address::ZERO, B256::ZERO)
        } else {
            Self::sender_tag(event.sender, tx_hash)
        };

        Self {
            token: event.token,
            senderTag: sender_tag,
            to: event.to,
            amount: event.amount,
            memo: event.memo,
            gasLimit: event.gasLimit,
            fallbackNonce: event.fallbackNonce,
            callbackData: event.data.clone(),
            encryptedSender: encrypted_sender,
        }
    }

    /// Hash this withdrawal as one link in a withdrawal queue.
    pub fn hash_with_tail(&self, tail: B256) -> B256 {
        keccak256((self.clone(), tail).abi_encode_params())
    }

    /// Compute the withdrawal queue hash for a slice of withdrawals.
    ///
    /// The hash chain has the oldest withdrawal at the outermost layer for efficient FIFO removal:
    ///
    /// ```text
    /// hash = keccak256(encode(w[0], keccak256(encode(w[1], keccak256(encode(w[2], EMPTY_SENTINEL))))))
    /// ```
    ///
    /// Building proceeds from the newest (innermost) to the oldest (outermost).
    /// Returns `B256::ZERO` if `withdrawals` is empty.
    pub fn queue_hash(withdrawals: &[Self]) -> B256 {
        if withdrawals.is_empty() {
            return B256::ZERO;
        }

        let mut hash = EMPTY_SENTINEL;
        for withdrawal in withdrawals.iter().rev() {
            hash = withdrawal.hash_with_tail(hash);
        }
        hash
    }
}
