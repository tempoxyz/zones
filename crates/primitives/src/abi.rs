//! ABI bindings for the Tempo Zone protocol contracts.
//!
//! These bindings cover the two main contracts the sequencer interacts with:
//!
//! - **ZonePortal** — deployed on Tempo L1. Escrows gas tokens, manages the deposit queue,
//!   accepts batch proofs, and processes withdrawals back to L1 recipients.
//!
//! - **ZoneOutbox** — deployed on the Zone L2. Collects user withdrawal requests, builds
//!   withdrawal hash chains, and exposes [`LastBatch`] state for proof generation.

// The `sol!` macro generates functions whose arity we don't control.
#![allow(clippy::too_many_arguments)]

use alloc::vec::Vec;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::SolValue;

pub use crate::constants::{
    EMPTY_SENTINEL, PORTAL_PENDING_SEQUENCER_SLOT, PORTAL_SEQUENCER_SLOT, TEMPO_BLOCK_HASH_SLOT,
    TEMPO_PACKED_SLOT, TEMPO_STATE_ADDRESS, TEMPO_STATE_READER_ADDRESS, ZONE_CONFIG_ADDRESS,
    ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZONE_TOKEN_ADDRESS, ZONE_TX_CONTEXT_ADDRESS,
};

/// Internal macro that emits the full `sol!` block, placing `$($rpc_attr)*`
/// before every `contract` declaration.  Called twice: once with `#[sol(rpc)]`
/// (when the `rpc` feature is active) and once with nothing.
macro_rules! define_abi {
    ($($rpc_attr:tt)*) => {
        alloy_sol_types::sol! {
            // ---------------------------------------------------------------
            //  Shared types
            // ---------------------------------------------------------------

            #[derive(Debug)]
            struct Withdrawal {
                address token;
                bytes32 senderTag;
                address to;
                uint128 amount;
                uint128 fee;
                bytes32 memo;
                uint64 gasLimit;
                address fallbackRecipient;
                bytes callbackData;
                bytes encryptedSender;
            }

            #[derive(Debug)]
            struct Deposit {
                address token;
                address sender;
                address to;
                uint128 amount;
                bytes32 memo;
            }

            /// Encrypted deposit payload (ECIES encrypted recipient and memo)
            #[derive(Debug)]
            struct EncryptedDepositPayload {
                bytes32 ephemeralPubkeyX;
                uint8 ephemeralPubkeyYParity;
                bytes ciphertext;
                bytes12 nonce;
                bytes16 tag;
            }

            /// Encrypted deposit stored in the queue
            #[derive(Debug)]
            struct EncryptedDeposit {
                address token;
                address sender;
                uint128 amount;
                uint256 keyIndex;
                EncryptedDepositPayload encrypted;
            }

            #[derive(Debug)]
            struct BlockTransition {
                bytes32 prevBlockHash;
                bytes32 nextBlockHash;
            }

            #[derive(Debug)]
            struct DepositQueueTransition {
                bytes32 prevProcessedHash;
                bytes32 nextProcessedHash;
            }

            #[derive(Debug)]
            struct LastBatch {
                bytes32 withdrawalQueueHash;
                uint64 withdrawalBatchIndex;
            }

            /// A TIP-20 token enabled on L1 for bridging to the zone.
            #[derive(Debug)]
            struct EnabledToken {
                address token;
                string name;
                string symbol;
                string currency;
            }

            /// Generic unauthorized access error used by zone wrapper logic.
            error Unauthorized();

            // ---------------------------------------------------------------
            //  ZonePortal — deployed on Tempo L1
            // ---------------------------------------------------------------

            $($rpc_attr)*
            contract ZonePortal {
                // -- Events --

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

                #[derive(Debug)]
                event BatchSubmitted(
                    uint64 indexed withdrawalBatchIndex,
                    bytes32 nextProcessedDepositQueueHash,
                    bytes32 nextBlockHash,
                    bytes32 withdrawalQueueHash
                );

                #[derive(Debug)]
                event WithdrawalProcessed(address indexed to, address token, uint128 amount, bool callbackSuccess);

                #[derive(Debug)]
                event BounceBack(
                    bytes32 indexed newCurrentDepositQueueHash,
                    address indexed fallbackRecipient,
                    address token,
                    uint128 amount
                );

                #[derive(Debug)]
                event SequencerTransferStarted(
                    address indexed currentSequencer,
                    address indexed pendingSequencer
                );

                #[derive(Debug)]
                event SequencerTransferred(
                    address indexed previousSequencer,
                    address indexed newSequencer
                );

                // -- Errors --

                #[derive(Debug)]
                error NotSequencer();
                #[derive(Debug)]
                error InvalidProof();
                #[derive(Debug)]
                error InvalidTempoBlockNumber();
                #[derive(Debug)]
                error DepositPolicyForbids();

                // -- View functions --

                function zoneId() external view returns (uint32);
                function sequencer() external view returns (address);
                function zoneFactory() external view returns (address);
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

                // -- State-changing functions --

                function deposit(address token, address to, uint128 amount, bytes32 memo)
                    external
                    returns (bytes32 newCurrentDepositQueueHash);

                function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external;

                function submitBatch(
                    address targetVerifier,
                    uint64 tempoBlockNumber,
                    uint64 recentTempoBlockNumber,
                    BlockTransition calldata blockTransition,
                    DepositQueueTransition calldata depositQueueTransition,
                    bytes32 withdrawalQueueHash,
                    bytes calldata verifierConfig,
                    bytes calldata proof
                ) external;

                function enableToken(address token) external;

                function depositEncrypted(
                    address token,
                    uint128 amount,
                    uint256 keyIndex,
                    EncryptedDepositPayload calldata encrypted
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

                function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);

                function encryptionKeyCount() external view returns (uint256);
            }

            // ---------------------------------------------------------------
            //  ZoneOutbox — deployed on Zone L2
            // ---------------------------------------------------------------

            $($rpc_attr)*
            contract ZoneOutbox {
                // -- Events --

                event WithdrawalRequested(
                    uint64 indexed withdrawalIndex,
                    address indexed sender,
                    address token,
                    address to,
                    uint128 amount,
                    uint128 fee,
                    bytes32 memo,
                    uint64 gasLimit,
                    address fallbackRecipient,
                    bytes data,
                    bytes revealTo
                );

                #[derive(Debug)]
                event BatchFinalized(bytes32 indexed withdrawalQueueHash, uint64 withdrawalBatchIndex);

                // -- Errors --

                error OnlySequencer();

                // -- View functions --

                function lastBatch() external view returns (LastBatch memory);
                function withdrawalBatchIndex() external view returns (uint64);
                function nextWithdrawalIndex() external view returns (uint64);
                function pendingWithdrawalsCount() external view returns (uint256);
                function calculateWithdrawalFee(uint64 gasLimit) external view returns (uint128 fee);

                // -- State-changing functions --

                function requestWithdrawal(
                    address token,
                    address to,
                    uint128 amount,
                    bytes32 memo,
                    uint64 gasLimit,
                    address fallbackRecipient,
                    bytes calldata data,
                    bytes calldata revealTo
                ) external;
                function finalizeWithdrawalBatch(uint256 count, uint64 blockNumber, bytes[] calldata encryptedSenders) external returns (bytes32 withdrawalQueueHash);
            }

            // ---------------------------------------------------------------
            //  TempoState — Zone L2 predeploy (0x1c00...0000)
            // ---------------------------------------------------------------

            $($rpc_attr)*
            contract TempoState {
                #[derive(Debug)]
                event TempoBlockFinalized(bytes32 indexed blockHash, uint64 indexed blockNumber, bytes32 stateRoot);

                error InvalidParentHash();
                error InvalidBlockNumber();
                error InvalidRlpData();
                error OnlyZoneInbox();

                function tempoBlockHash() external view returns (bytes32);
                function tempoBlockNumber() external view returns (uint64);
                function tempoStateRoot() external view returns (bytes32);
                function tempoParentHash() external view returns (bytes32);
                function tempoBeneficiary() external view returns (address);
                function tempoTransactionsRoot() external view returns (bytes32);
                function tempoReceiptsRoot() external view returns (bytes32);
                function tempoGasLimit() external view returns (uint64);
                function tempoGasUsed() external view returns (uint64);
                function tempoTimestamp() external view returns (uint64);
                function tempoTimestampMillis() external view returns (uint64);
                function tempoPrevRandao() external view returns (bytes32);
                function generalGasLimit() external view returns (uint64);
                function sharedGasLimit() external view returns (uint64);

                function finalizeTempo(bytes calldata header) external;
            }

            // ---------------------------------------------------------------
            //  TempoStateReader — Zone L2 standalone precompile
            //  Separate from TempoState; reads Tempo L1 storage at a caller-specified block.
            // ---------------------------------------------------------------

            $($rpc_attr)*
            contract TempoStateReader {
                error DelegateCallNotAllowed();

                function readStorageAt(address account, bytes32 slot, uint64 blockNumber) external view returns (bytes32);
                function readStorageBatchAt(address account, bytes32[] calldata slots, uint64 blockNumber) external view returns (bytes32[] memory);
            }

            $($rpc_attr)*
            contract ZoneTxContext {
                function currentTxHash() external returns (bytes32);
            }

            // ---------------------------------------------------------------
            //  ZoneInbox — Zone L2 system contract (0x1c00...0001)
            // ---------------------------------------------------------------

            /// Deposit types for the unified deposit queue.
            #[derive(Debug, PartialEq, Eq)]
            enum DepositType {
                Regular,
                Encrypted,
            }

            /// A queued deposit (regular or encrypted) passed to `advanceTempo`.
            #[derive(Debug)]
            struct QueuedDeposit {
                DepositType depositType;
                bytes depositData;
            }

            /// Chaum-Pedersen proof for ECDH shared secret derivation.
            #[derive(Debug)]
            struct ChaumPedersenProof {
                bytes32 s;
                bytes32 c;
            }

            /// Decryption data provided by the sequencer for encrypted deposits.
            #[derive(Debug)]
            struct DecryptionData {
                bytes32 sharedSecret;
                uint8 sharedSecretYParity;
                address to;
                bytes32 memo;
                ChaumPedersenProof cpProof;
            }

            // ---------------------------------------------------------------
            //  ZoneFactory — deployed on Tempo L1
            // ---------------------------------------------------------------

            #[derive(Debug)]
            struct ZoneInfo {
                uint32 zoneId;
                address portal;
                address messenger;
                address initialToken;
                address sequencer;
                bytes32 genesisBlockHash;
                bytes32 genesisTempoBlockHash;
                uint64 genesisTempoBlockNumber;
            }

            $($rpc_attr)*
            contract ZoneFactory {
                struct ZoneParams {
                    bytes32 genesisBlockHash;
                    bytes32 genesisTempoBlockHash;
                    uint64 genesisTempoBlockNumber;
                }
                struct CreateZoneParams {
                    address token;
                    address sequencer;
                    ZoneParams zoneParams;
                }
                #[derive(Debug)]
                event ZoneCreated(
                    uint32 indexed zoneId,
                    address indexed portal,
                    address indexed messenger,
                    address token,
                    address sequencer,
                    bytes32 genesisBlockHash,
                    bytes32 genesisTempoBlockHash,
                    uint64 genesisTempoBlockNumber
                );
                function createZone(CreateZoneParams calldata params) external returns (uint32 zoneId, address portal);
                function verifier() external view returns (address);
                function forkVerifier() external view returns (address);
                function forkActivationBlock() external view returns (uint64);
                function protocolVersion() external view returns (uint64);
                function setForkVerifier(address newForkVerifier) external;
                function validateVerifier(address targetVerifier, uint64 tempoBlockNumber) external view;
                function zones(uint32 zoneId) external view returns (ZoneInfo memory);
                function zoneCount() external view returns (uint32);
                function isZonePortal(address portal) external view returns (bool);
                function isZoneMessenger(address messenger) external view returns (bool);
            }

            // ---------------------------------------------------------------
            //  ZoneInbox — Zone L2 system contract (0x1c00...0001)
            // ---------------------------------------------------------------

            $($rpc_attr)*
            contract ZoneInbox {
                #[derive(Debug)]
                event TempoAdvanced(
                    bytes32 indexed tempoBlockHash,
                    uint64 indexed tempoBlockNumber,
                    uint256 depositsProcessed,
                    bytes32 newProcessedDepositQueueHash
                );

                #[derive(Debug)]
                event DepositProcessed(
                    bytes32 indexed depositHash,
                    address indexed sender,
                    address indexed to,
                    address token,
                    uint128 amount,
                    bytes32 memo
                );

                #[derive(Debug)]
                event EncryptedDepositProcessed(
                    bytes32 indexed depositHash,
                    address indexed sender,
                    address indexed to,
                    address token,
                    uint128 amount,
                    bytes32 memo
                );

                #[derive(Debug)]
                event EncryptedDepositFailed(
                    bytes32 indexed depositHash,
                    address indexed sender,
                    address token,
                    uint128 amount
                );

                /// Emitted when a TIP-20 token is enabled on the zone via advanceTempo.
                #[derive(Debug)]
                event TokenEnabled(address indexed token, string name, string symbol, string currency);

                error OnlySequencer();
                error InvalidDepositQueueHash();
                error MissingDecryptionData();
                error ExtraDecryptionData();
                error InvalidSharedSecretProof();
                function processedDepositQueueHash() external view returns (bytes32);
                function tempoPortal() external view returns (address);
                function tempoState() external view returns (address);
                function config() external view returns (address);

                function advanceTempo(
                    bytes calldata header,
                    QueuedDeposit[] calldata deposits,
                    DecryptionData[] calldata decryptions,
                    EnabledToken[] calldata enabledTokens
                ) external;
            }

            // ---------------------------------------------------------------
            //  SwapAndDepositRouter — deployed on Tempo L1
            // ---------------------------------------------------------------

            $($rpc_attr)*
            contract SwapAndDepositRouter {
                function onWithdrawalReceived(
                    bytes32 senderTag,
                    address tokenIn,
                    uint128 amount,
                    bytes calldata data
                ) external returns (bytes4);
            }
        }
    };
}

#[cfg(feature = "rpc")]
define_abi!(#[sol(rpc)]);

#[cfg(not(feature = "rpc"))]
define_abi!();

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

/// Plaintext callback payload for `SwapAndDepositRouter.onWithdrawalReceived`.
///
/// This payload tells the router to optionally swap the withdrawn token on L1
/// and then perform a regular `ZonePortal.deposit(...)`.
#[derive(Debug, Clone)]
pub struct SwapAndDepositRouterPlaintextCallback {
    /// Token that should be deposited after the optional L1 swap.
    pub token_out: Address,
    /// Target zone portal that receives the downstream deposit.
    pub target_portal: Address,
    /// Zone recipient for the downstream plaintext deposit.
    pub recipient: Address,
    /// Memo recorded on the downstream plaintext deposit.
    pub memo: B256,
    /// Minimum acceptable output from the optional swap.
    ///
    /// Ignored when `tokenIn == token_out` and the router can deposit directly.
    pub min_amount_out: u128,
}

impl SwapAndDepositRouterPlaintextCallback {
    /// ABI-encode the router callback data expected by the Solidity router.
    pub fn abi_encode(&self) -> Vec<u8> {
        (
            false,
            self.token_out,
            self.target_portal,
            self.recipient,
            self.memo,
            self.min_amount_out,
        )
            .abi_encode_params()
    }
}

/// Encrypted callback payload for `SwapAndDepositRouter.onWithdrawalReceived`.
///
/// This payload tells the router to optionally swap the withdrawn token on L1
/// and then call `ZonePortal.depositEncrypted(...)` with an ECIES-encrypted
/// `(recipient, memo)` payload.
#[derive(Debug, Clone)]
pub struct SwapAndDepositRouterEncryptedCallback {
    /// Token that should be deposited after the optional L1 swap.
    pub token_out: Address,
    /// Target zone portal that receives the downstream encrypted deposit.
    pub target_portal: Address,
    /// Portal encryption key index used to build [`Self::encrypted`].
    pub key_index: U256,
    /// ECIES-encrypted `(recipient, memo)` payload for `depositEncrypted`.
    pub encrypted: EncryptedDepositPayload,
    /// Minimum acceptable output from the optional swap.
    ///
    /// Ignored when `tokenIn == token_out` and the router can deposit directly.
    pub min_amount_out: u128,
}

impl SwapAndDepositRouterEncryptedCallback {
    /// ABI-encode the router callback data expected by the Solidity router.
    pub fn abi_encode(&self) -> Vec<u8> {
        (
            true,
            self.token_out,
            self.target_portal,
            self.key_index,
            self.encrypted.clone(),
            self.min_amount_out,
        )
            .abi_encode_params()
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

impl core::fmt::Display for ZonePortal::ZonePortalErrors {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotSequencer(_) => f.write_str("NotSequencer"),
            Self::InvalidProof(_) => f.write_str("InvalidProof"),
            Self::InvalidTempoBlockNumber(_) => f.write_str("InvalidTempoBlockNumber"),
            Self::DepositPolicyForbids(_) => f.write_str("DepositPolicyForbids"),
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
        Self {
            token: event.token,
            senderTag: Self::sender_tag(event.sender, tx_hash),
            to: event.to,
            amount: event.amount,
            fee: event.fee,
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, address};
    use alloy_sol_types::SolCall;

    #[test]
    fn test_deposit_abi_encode_vs_params() {
        let d = Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000u128,
            memo: B256::ZERO,
        };

        let encoded = d.abi_encode();
        let encoded_params = d.abi_encode_params();

        println!("abi_encode length: {}", encoded.len());
        println!("abi_encode_params length: {}", encoded_params.len());
        println!("abi_encode hex:\n{}", const_hex::encode(&encoded));
        println!(
            "abi_encode_params hex:\n{}",
            const_hex::encode(&encoded_params)
        );
        println!("Are they equal: {}", encoded == encoded_params);
    }

    #[test]
    fn test_queued_deposit_encoding() {
        let deposit = Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000u128,
            memo: B256::ZERO,
        };

        let deposit_data = Bytes::from(deposit.abi_encode());

        let qd = QueuedDeposit {
            depositType: DepositType::Regular,
            depositData: deposit_data,
        };

        println!(
            "DepositType::Regular abi_encode: {}",
            const_hex::encode(DepositType::Regular.abi_encode())
        );
        println!(
            "deposit.abi_encode() length: {}",
            deposit.abi_encode().len()
        );
        println!(
            "deposit.abi_encode(): {}",
            const_hex::encode(deposit.abi_encode())
        );
        println!(
            "QueuedDeposit.abi_encode() length: {}",
            qd.abi_encode().len()
        );
        println!(
            "QueuedDeposit.abi_encode(): {}",
            const_hex::encode(qd.abi_encode())
        );

        // Now test the full advanceTempo call encoding
        let header_bytes = Bytes::from(vec![0xc0]); // minimal RLP empty list
        let calldata = ZoneInbox::advanceTempoCall {
            header: header_bytes,
            deposits: vec![qd],
            decryptions: vec![],
            enabledTokens: vec![],
        }
        .abi_encode();

        println!("\nadvanceTempo calldata length: {}", calldata.len());
        println!(
            "advanceTempo selector: 0x{}",
            const_hex::encode(&calldata[..4])
        );
        println!(
            "advanceTempo full calldata:\n{}",
            const_hex::encode(&calldata)
        );
    }

    #[test]
    fn test_deposit_hash_chain_matches_solidity() {
        let deposit = Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000u128,
            memo: B256::ZERO,
        };
        let prev_hash = B256::ZERO;

        let solidity_encoding = (DepositType::Regular, deposit.clone(), prev_hash).abi_encode();
        let solidity_hash = keccak256(&solidity_encoding);

        let rust_encoding = (DepositType::Regular, deposit, prev_hash).abi_encode();
        let rust_hash = keccak256(&rust_encoding);

        assert_eq!(solidity_encoding, rust_encoding, "ABI encodings must match");
        assert_eq!(solidity_hash, rust_hash, "Deposit hash chains must match");
    }

    #[test]
    fn test_router_plaintext_callback_encoding_matches_tuple() {
        let callback = SwapAndDepositRouterPlaintextCallback {
            token_out: address!("0x0000000000000000000000000000000000001001"),
            target_portal: address!("0x0000000000000000000000000000000000002001"),
            recipient: address!("0x0000000000000000000000000000000000003001"),
            memo: B256::from([0x11; 32]),
            min_amount_out: 1234,
        };

        let tuple_encoding = (
            false,
            callback.token_out,
            callback.target_portal,
            callback.recipient,
            callback.memo,
            callback.min_amount_out,
        )
            .abi_encode_params();

        assert_eq!(callback.abi_encode(), tuple_encoding);
    }

    #[test]
    fn test_sender_tag_matches_plaintext_hash() {
        let sender = address!("0x0000000000000000000000000000000000000001");
        let tx_hash = B256::repeat_byte(0x22);
        let plaintext = Withdrawal::authenticated_sender_plaintext(sender, tx_hash);

        assert_eq!(&plaintext[..20], sender.as_slice());
        assert_eq!(&plaintext[20..], tx_hash.as_slice());
        assert_eq!(
            Withdrawal::sender_tag(sender, tx_hash),
            keccak256(plaintext)
        );
    }

    #[test]
    fn test_router_encrypted_callback_encoding_matches_tuple() {
        let encrypted = EncryptedDepositPayload {
            ephemeralPubkeyX: B256::from([0x22; 32]),
            ephemeralPubkeyYParity: 0x02,
            ciphertext: Bytes::from(vec![0xaa, 0xbb, 0xcc, 0xdd]),
            nonce: [0x33; 12].into(),
            tag: [0x44; 16].into(),
        };
        let callback = SwapAndDepositRouterEncryptedCallback {
            token_out: address!("0x0000000000000000000000000000000000001002"),
            target_portal: address!("0x0000000000000000000000000000000000002002"),
            key_index: U256::from(7),
            encrypted: encrypted.clone(),
            min_amount_out: 5678,
        };

        let tuple_encoding = (
            true,
            callback.token_out,
            callback.target_portal,
            callback.key_index,
            encrypted,
            callback.min_amount_out,
        )
            .abi_encode_params();

        assert_eq!(callback.abi_encode(), tuple_encoding);
    }
}
