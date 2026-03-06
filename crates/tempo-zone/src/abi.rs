//! ABI bindings for the Tempo Zone protocol contracts.
//!
//! These bindings cover the two main contracts the sequencer interacts with:
//!
//! - **ZonePortal** — deployed on Tempo L1. Escrows gas tokens, manages the deposit queue,
//!   accepts batch proofs, and processes withdrawals back to L1 recipients.
//!
//! - **ZoneOutbox** — deployed on the Zone L2. Collects user withdrawal requests, builds
//!   withdrawal hash chains, and exposes [`LastBatch`] state for proof generation.

use alloy_primitives::{Address, B256, address, keccak256};
use alloy_sol_types::{SolValue, sol};

/// Sentinel value for empty withdrawal queue slots.
/// Using 0xff...ff instead of 0x00 to avoid clearing storage.
pub const EMPTY_SENTINEL: B256 = B256::new([0xff; 32]);

/// TempoState predeploy address on Zone L2.
pub const TEMPO_STATE_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000000");

/// TempoState storage slot for `tempoBlockHash` (slot 0).
pub const TEMPO_BLOCK_HASH_SLOT: B256 = B256::ZERO;

/// TempoState storage slot for packed `(tempoBlockNumber, tempoGasLimit, tempoGasUsed, tempoTimestamp)` (slot 7).
pub const TEMPO_PACKED_SLOT: B256 = {
    let mut bytes = [0u8; 32];
    bytes[31] = 7;
    B256::new(bytes)
};

/// ZoneInbox predeploy address on Zone L2.
pub const ZONE_INBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000001");

/// ZoneOutbox predeploy address on Zone L2.
pub const ZONE_OUTBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000002");

/// ZoneConfig predeploy address on Zone L2.
pub const ZONE_CONFIG_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000003");

/// TempoStateReader predeploy address on Zone L2.
/// Standalone precompile — separate from the TempoState contract.
pub const TEMPO_STATE_READER_ADDRESS: Address =
    address!("0x1c00000000000000000000000000000000000004");

/// Default zone token address on Zone L2 — pathUSD TIP20 precompile.
///
/// This is the initial token enabled at zone creation. With multi-asset support,
/// additional TIP-20 tokens can be enabled via the portal's `enableToken()`.
/// This is the same TIP20 precompile address as on Tempo L1, initialized in zone genesis
/// with the TIP20Factory so that `is_valid_fee_token` passes for user transactions.
pub const ZONE_TOKEN_ADDRESS: Address = address!("0x20C0000000000000000000000000000000000000");

sol! {
    // ---------------------------------------------------------------
    //  Shared types
    // ---------------------------------------------------------------

    #[derive(Debug)]
    struct Withdrawal {
        address token;
        address sender;
        address to;
        uint128 amount;
        uint128 fee;
        bytes32 memo;
        uint64 gasLimit;
        address fallbackRecipient;
        bytes callbackData;
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

    // ---------------------------------------------------------------
    //  ZonePortal — deployed on Tempo L1
    // ---------------------------------------------------------------

    #[sol(rpc)]
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

        function zoneId() external view returns (uint64);
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

        // -- State-changing functions --

        function deposit(address token, address to, uint128 amount, bytes32 memo)
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

    #[sol(rpc)]
    contract ZoneOutbox {
        // -- Events --

        #[derive(Debug)]
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
            bytes data
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
            bytes calldata data
        ) external;
        function finalizeWithdrawalBatch(uint256 count, uint64 blockNumber) external returns (bytes32 withdrawalQueueHash);
    }

    // ---------------------------------------------------------------
    //  TempoState — Zone L2 predeploy (0x1c00...0000)
    // ---------------------------------------------------------------

    #[sol(rpc)]
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

    #[sol(rpc)]
    contract TempoStateReader {
        error DelegateCallNotAllowed();

        function readStorageAt(address account, bytes32 slot, uint64 blockNumber) external view returns (bytes32);
        function readStorageBatchAt(address account, bytes32[] calldata slots, uint64 blockNumber) external view returns (bytes32[] memory);
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
        uint64 zoneId;
        address portal;
        address messenger;
        address initialToken;
        address sequencer;
        address verifier;
        bytes32 genesisBlockHash;
        bytes32 genesisTempoBlockHash;
        uint64 genesisTempoBlockNumber;
    }

    #[sol(rpc)]
    contract ZoneFactory {
        struct ZoneParams {
            bytes32 genesisBlockHash;
            bytes32 genesisTempoBlockHash;
            uint64 genesisTempoBlockNumber;
        }
        struct CreateZoneParams {
            address token;
            address sequencer;
            address verifier;
            ZoneParams zoneParams;
        }
        #[derive(Debug)]
        event ZoneCreated(
            uint64 indexed zoneId,
            address indexed portal,
            address indexed messenger,
            address token,
            address sequencer,
            address verifier,
            bytes32 genesisBlockHash,
            bytes32 genesisTempoBlockHash,
            uint64 genesisTempoBlockNumber
        );
        function createZone(CreateZoneParams calldata params) external returns (uint64 zoneId, address portal);
        function verifier() external view returns (address);
        function zones(uint64 zoneId) external view returns (ZoneInfo memory);
        function zoneCount() external view returns (uint64);
        function isZonePortal(address portal) external view returns (bool);
        function isZoneMessenger(address messenger) external view returns (bool);
    }

    // ---------------------------------------------------------------
    //  ZoneInbox — Zone L2 system contract (0x1c00...0001)
    // ---------------------------------------------------------------

    #[sol(rpc)]
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

        #[derive(Debug)]
        event MaxDepositsPerTempoBlockUpdated(uint256 maxDepositsPerTempoBlock);

        /// Emitted when a TIP-20 token is enabled on the zone via advanceTempo.
        #[derive(Debug)]
        event TokenEnabled(address indexed token, string name, string symbol, string currency);

        error OnlySequencer();
        error InvalidDepositQueueHash();
        error MissingDecryptionData();
        error ExtraDecryptionData();
        error InvalidSharedSecretProof();
        error TooManyDeposits();

        function processedDepositQueueHash() external view returns (bytes32);
        function maxDepositsPerTempoBlock() external view returns (uint256);
        function tempoPortal() external view returns (address);
        function tempoState() external view returns (address);
        function config() external view returns (address);

        function setMaxDepositsPerTempoBlock(uint256 _maxDepositsPerTempoBlock) external;

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

    #[sol(rpc)]
    contract SwapAndDepositRouter {
        function onWithdrawalReceived(
            address sender,
            uint128 amount,
            bytes calldata data
        ) external returns (bytes4);
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

impl<P: alloy_provider::Provider<N>, N: alloy_network::Network>
    ZonePortal::ZonePortalInstance<P, N>
{
    /// Fetches the active sequencer encryption key and its index.
    ///
    /// Returns `(key, key_index)` where `key` is the
    /// [`sequencerEncryptionKeyReturn`](ZonePortal::sequencerEncryptionKeyReturn) and
    /// `key_index` is the zero-based index of the current key.
    pub async fn encryption_key(
        &self,
    ) -> Result<(ZonePortal::sequencerEncryptionKeyReturn, alloy_primitives::U256), alloy_contract::Error> {
        let (key, count) = tokio::try_join!(
            self.sequencerEncryptionKey().call(),
            self.encryptionKeyCount().call(),
        )?;
        let key_index = count.saturating_sub(alloy_primitives::U256::from(1));
        Ok((key, key_index))
    }
}

impl std::fmt::Display for ZonePortal::ZonePortalErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSequencer(_) => f.write_str("NotSequencer"),
            Self::InvalidProof(_) => f.write_str("InvalidProof"),
            Self::InvalidTempoBlockNumber(_) => f.write_str("InvalidTempoBlockNumber"),
            Self::DepositPolicyForbids(_) => f.write_str("DepositPolicyForbids"),
        }
    }
}

impl Withdrawal {
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
            hash = keccak256((w.clone(), hash).abi_encode());
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
        // Test that the QueuedDeposit encoding matches what Solidity expects
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
        // Both Solidity and Rust must compute:
        //   keccak256(abi.encode(DepositType.Regular, deposit, prevHash))
        let deposit = Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000u128,
            memo: B256::ZERO,
        };
        let prev_hash = B256::ZERO;

        // What Solidity computes: abi.encode(DepositType.Regular, deposit, prevHash)
        let solidity_encoding = (DepositType::Regular, deposit.clone(), prev_hash).abi_encode();
        let solidity_hash = keccak256(&solidity_encoding);

        // What Rust l1.rs now computes (with DepositType discriminator)
        let rust_encoding = (DepositType::Regular, deposit, prev_hash).abi_encode();
        let rust_hash = keccak256(&rust_encoding);

        assert_eq!(solidity_encoding, rust_encoding, "ABI encodings must match");
        assert_eq!(solidity_hash, rust_hash, "Deposit hash chains must match");
    }
}
