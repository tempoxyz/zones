//! `ZonePortal` — deployed on Tempo L1.

pub use ZonePortal::{
    BlockTransition, DepositQueueTransition, EncryptedDeposit, EncryptedDepositPayload, Withdrawal,
};

use crate::ZoneOutbox;
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_sol_types::SolValue;
use zone_primitives::constants::EMPTY_SENTINEL;

crate::sol! {
    #[derive(Debug)]
    contract ZonePortal {
        // -- Shared types --

        struct Withdrawal {
            address token;
            bytes32 senderTag;
            address to;
            uint128 amount;
            uint128 fee;
            uint128 bouncebackFee;
            bytes32 memo;
            uint64 gasLimit;
            address fallbackRecipient;
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
            address bouncebackRecipient;
            uint128 bouncebackFee;
            uint256 keyIndex;
            EncryptedDepositPayload encrypted;
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
            uint128 bouncebackFee,
            bytes32 memo,
            address bouncebackRecipient,
            uint64 depositNumber
        );

        event EncryptedDepositMade(
            bytes32 indexed newCurrentDepositQueueHash,
            address indexed sender,
            address token,
            uint128 netAmount,
            uint128 fee,
            uint128 bouncebackFee,
            uint256 keyIndex,
            bytes32 ephemeralPubkeyX,
            uint8 ephemeralPubkeyYParity,
            bytes ciphertext,
            bytes12 nonce,
            bytes16 tag,
            address bouncebackRecipient,
            uint64 depositNumber
        );

        /// Event emitted when a new TIP-20 token is enabled for bridging.
        /// Includes token metadata so the zone can create a matching TIP-20.
        event TokenEnabled(address indexed token, string name, string symbol, string currency);

        event BatchSubmitted(
            uint64 indexed withdrawalBatchIndex,
            bytes32 nextProcessedDepositQueueHash,
            bytes32 nextBlockHash,
            bytes32 withdrawalQueueHash,
            uint64 lastProcessedDepositNumber
        );

        event WithdrawalProcessed(address indexed to, address token, uint128 amount, bool callbackSuccess);

        event WithdrawalBounceBack(
            bytes32 indexed newCurrentDepositQueueHash,
            address indexed fallbackRecipient,
            address token,
            uint128 amount,
            uint64 depositNumber
        );

        event DepositBounceBack(
            address indexed bouncebackRecipient,
            address token,
            uint128 amount,
            uint128 bouncebackFee
        );

        event DepositBounceBackPending(
            address indexed bouncebackRecipient,
            address token,
            uint128 amount,
            uint128 bouncebackFee
        );

        event RefundClaimed(address indexed recipient, address indexed token, uint128 amount);

        event SequencerTransferStarted(
            address indexed currentSequencer,
            address indexed pendingSequencer
        );

        event SequencerTransferred(
            address indexed previousSequencer,
            address indexed newSequencer
        );

        // -- Errors --

        error NotSequencer();
        error InvalidProof();
        error InvalidTempoBlockNumber();
        error PolicyForbids();
        error InvalidBouncebackRecipient();

        // -- View functions --

        function zoneId() external view returns (uint32);
        function sequencer() external view returns (address);
        function verifier() external view returns (address);
        function sequencerPubkey() external view returns (bytes32);
        function withdrawalBatchIndex() external view returns (uint64);
        function blockHash() external view returns (bytes32);
        function currentDepositQueueHash() external view returns (bytes32);
        function lastSyncedTempoBlockNumber() external view returns (uint64);
        function withdrawalQueueHead() external view returns (uint256);
        function withdrawalQueueTail() external view returns (uint256);
        function withdrawalQueueMaxSize() external view returns (uint256);
        function withdrawalQueueSlot(uint256 slot) external view returns (bytes32);
        function genesisTempoBlockNumber() external view returns (uint64);
        function calculateDepositFee() external view returns (uint128 fee);
        function calculateBouncebackFee() external view returns (uint128 fee);
        function depositCount() external view returns (uint64);
        function lastProcessedDepositNumber() external view returns (uint64);
        function MAX_WITHDRAWAL_GAS_LIMIT() external view returns (uint64);

        // -- State-changing functions --

        function deposit(
            address token,
            address to,
            uint128 amount,
            bytes32 memo,
            address bouncebackRecipient
        )
            external
            returns (bytes32 newCurrentDepositQueueHash);

        function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external;

        function submitBatch(
            uint64 tempoBlockNumber,
            uint64 recentTempoBlockNumber,
            BlockTransition calldata blockTransition,
            DepositQueueTransition calldata depositQueueTransition,
            bytes32 withdrawalQueueHash,
            bytes calldata verifierConfig,
            bytes calldata proof
        ) external;

        function enableToken(address token) external;

        function rpcUrl() external view returns (string memory);
        function setRpcUrl(string calldata rpcUrl) external;

        function depositEncrypted(
            address token,
            uint128 amount,
            uint256 keyIndex,
            EncryptedDepositPayload calldata encrypted,
            address bouncebackRecipient
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
        function pendingSequencer() external view returns (address);
        function refunds(address token, address owner) external view returns (uint128);

        function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);

        function encryptionKeyCount() external view returns (uint256);
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
        let count = self.enabledTokenCount().call().await?;
        let futs: alloc::vec::Vec<_> = (0..count.to::<u64>())
            .map(|i| async move {
                self.enabledTokenAt(alloy_primitives::U256::from(i))
                    .call()
                    .await
            })
            .collect();
        futures::future::try_join_all(futs).await
    }

    /// Fetches the active sequencer encryption key and its index.
    ///
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
        let key_call = self.sequencerEncryptionKey();
        let count_call = self.encryptionKeyCount();
        let (key, count) = tokio::try_join!(key_call.call(), count_call.call())?;
        let key_index = count.saturating_sub(alloy_primitives::U256::from(1));
        Ok((key, key_index))
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
            Self::InvalidProof(_) => f.write_str("InvalidProof"),
            Self::InvalidTempoBlockNumber(_) => f.write_str("InvalidTempoBlockNumber"),
            Self::PolicyForbids(_) => f.write_str("PolicyForbids"),
            Self::InvalidBouncebackRecipient(_) => f.write_str("InvalidBouncebackRecipient"),
        }
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
        event: &ZoneOutbox::WithdrawalRequested,
        tx_hash: B256,
        encrypted_sender: Bytes,
    ) -> Self {
        let sender_tag = if event.sender.is_zero() && event.fallbackRecipient.is_zero() {
            Self::sender_tag(Address::ZERO, B256::ZERO)
        } else {
            Self::sender_tag(event.sender, tx_hash)
        };

        Self {
            token: event.token,
            senderTag: sender_tag,
            to: event.to,
            amount: event.amount,
            fee: event.fee,
            bouncebackFee: event.bouncebackFee,
            memo: event.memo,
            gasLimit: event.gasLimit,
            fallbackRecipient: event.fallbackRecipient,
            callbackData: event.data.clone(),
            encryptedSender: encrypted_sender,
        }
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
        for w in withdrawals.iter().rev() {
            hash = keccak256((w.clone(), hash).abi_encode_params());
        }
        hash
    }
}
