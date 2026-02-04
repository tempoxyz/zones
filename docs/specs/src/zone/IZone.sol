// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title IZoneToken
/// @notice Interface for the zone's zone token (TIP-20 with mint/burn for system)
interface IZoneToken {

    function mint(address to, uint256 amount) external;
    function burn(address from, uint256 amount) external;
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);

}

/// @notice Common types for the Zone protocol
struct ZoneInfo {
    uint64 zoneId;
    address portal;
    address messenger;
    address token;
    address sequencer;
    address verifier;
    bytes32 genesisBlockHash;
    bytes32 genesisTempoBlockHash;
    uint64 genesisTempoBlockNumber;
}

/// @notice Zone creation parameters stored in genesis
struct ZoneParams {
    bytes32 genesisBlockHash;
    bytes32 genesisTempoBlockHash;
    uint64 genesisTempoBlockNumber;
}

/// @notice Block transition for zone batch proofs
/// @dev Uses block hash instead of state root to commit to full block structure
///      (includes state root, transactions root, receipts root, etc.)
struct BlockTransition {
    bytes32 prevBlockHash;
    bytes32 nextBlockHash;
}

/// @notice Deposit queue transition inputs/outputs for batch proofs
/// @dev The proof reads currentDepositQueueHash from Tempo state to validate
///      that nextProcessedHash matches currentDepositQueueHash for now. TODO: allow ancestor checks.
struct DepositQueueTransition {
    bytes32 prevProcessedHash; // where proof starts (verified against zone state)
    bytes32 nextProcessedHash; // where zone processed up to (proof output)
}

/// @notice Withdrawal queue transition for batch proofs
/// @dev Each batch gets its own slot in an unbounded buffer.
///      The withdrawalQueueHash is the hash chain of withdrawals for this batch.
struct WithdrawalQueueTransition {
    bytes32 withdrawalQueueHash; // hash chain of withdrawals for this batch (0 if none)
}

/// @notice Deposit type discriminator for the unified deposit queue
/// @dev Used in hash chain: keccak256(abi.encode(depositType, depositData, prevHash))
enum DepositType {
    Regular,    // Standard deposit with plaintext recipient and memo
    Encrypted   // Encrypted deposit with hidden recipient and memo
}

struct Deposit {
    address sender;
    address to;
    uint128 amount;
    bytes32 memo;
}

/*//////////////////////////////////////////////////////////////
                        ENCRYPTED DEPOSITS
//////////////////////////////////////////////////////////////*/

/// @notice Encrypted deposit payload (recipient and memo encrypted to sequencer)
/// @dev Uses ECIES with secp256k1: ephemeral ECDH + AES-256-GCM
struct EncryptedDepositPayload {
    bytes32 ephemeralPubkeyX;     // Ephemeral public key X coordinate (for ECDH)
    uint8 ephemeralPubkeyYParity; // Y coordinate parity (0x02 or 0x03)
    bytes ciphertext;             // AES-256-GCM encrypted (to || memo || padding)
    bytes12 nonce;                // GCM nonce
    bytes16 tag;                  // GCM authentication tag
}

/// @notice Encrypted deposit stored in the queue
/// @dev Sender, amount, and key index are public; recipient and memo are encrypted.
///      The keyIndex specifies which encryption key the user used, allowing the prover
///      to look up the correct key for decryption even after key rotations.
struct EncryptedDeposit {
    address sender;              // Depositor (public, for refunds)
    uint128 amount;              // Amount (public, for accounting)
    uint256 keyIndex;            // Index of encryption key used (specified by depositor)
    EncryptedDepositPayload encrypted; // Encrypted (to, memo)
}

/// @notice Historical record of an encryption key with its activation block
/// @dev Used to track key rotations so the prover can determine which key
///      was valid when a deposit was made
struct EncryptionKeyEntry {
    bytes32 x;              // X coordinate of the public key
    uint8 yParity;          // Y coordinate parity (0x02 or 0x03)
    uint64 activationBlock; // Tempo block number when this key became active
}

/// Grace period after key rotation during which old keys are still accepted.
/// After this period, deposits using the old key are rejected.
/// 1 day at 1 second block time = 86400 blocks
uint64 constant ENCRYPTION_KEY_GRACE_PERIOD = 86400;

/*//////////////////////////////////////////////////////////////
                    UNIFIED DEPOSIT QUEUE TYPES
//////////////////////////////////////////////////////////////*/

/// @notice A deposit entry in the unified queue (for zone-side processing)
/// @dev Used by the sequencer when calling advanceTempo with mixed deposit types.
///      The depositData is ABI-encoded Deposit or EncryptedDeposit depending on type.
struct QueuedDeposit {
    DepositType depositType;
    bytes depositData;  // abi.encode(Deposit) or abi.encode(EncryptedDeposit)
}

/// @notice Decryption data provided by sequencer for encrypted deposits
/// @dev Must match 1:1 with encrypted deposits in the queue (in order of appearance).
///      The sharedSecret enables on-chain verification via GCM tag validation without
///      revealing the sequencer's private key.
struct DecryptionData {
    bytes32 sharedSecret;  // ECDH shared secret (sequencerPriv * ephemeralPub)
    address to;            // Decrypted recipient
    bytes32 memo;          // Decrypted memo
}

/*//////////////////////////////////////////////////////////////
                    ECIES VERIFICATION PRECOMPILE
//////////////////////////////////////////////////////////////*/

/// @notice Precompile address for ECIES decryption verification
/// @dev Predeploy at 0x1c00000000000000000000000000000000000100
address constant ECIES_VERIFY = 0x1c00000000000000000000000000000000000100;

/// @title IEciesVerify
/// @notice Precompile for verifying ECIES (secp256k1 + AES-256-GCM) decryption
/// @dev Validates that a given shared secret correctly decrypts a ciphertext by checking
///      the GCM authentication tag. Does not require the sequencer's private key.
interface IEciesVerify {
    /// @notice Verify that sharedSecret correctly decrypts the encrypted payload to plaintext
    /// @dev Uses HKDF-SHA256 to derive AES key from sharedSecret, then verifies GCM tag.
    ///      The GCM tag proves the shared secret is correct without revealing private keys.
    /// @param sharedSecret The ECDH shared secret (sequencerPriv * ephemeralPub)
    /// @param encrypted The encrypted payload (ephemeralPub, ciphertext, nonce, tag)
    /// @param plaintext The claimed decrypted data to verify
    /// @return valid True if the shared secret correctly decrypts to the plaintext
    function verifyDecryption(
        bytes32 sharedSecret,
        EncryptedDepositPayload calldata encrypted,
        bytes calldata plaintext
    ) external view returns (bool valid);
}

struct Withdrawal {
    address sender; // who initiated the withdrawal on the zone
    address to; // Tempo recipient
    uint128 amount; // amount to send to recipient (excludes fee)
    uint128 fee; // processing fee for sequencer (calculated at request time)
    bytes32 memo; // user-provided context
    uint64 gasLimit; // max gas for IWithdrawalReceiver callback (0 = no callback)
    address fallbackRecipient; // zone address for bounce-back if call fails
    bytes callbackData; // calldata for IWithdrawalReceiver (if gasLimit > 0)
}

/*//////////////////////////////////////////////////////////////
                    ZONE SYSTEM CONTRACTS
//////////////////////////////////////////////////////////////*/

// TempoState predeploy address (0x1c00...0000)
address constant TEMPO_STATE = 0x1C00000000000000000000000000000000000000;

// ZoneInbox system contract address (0x1c00...0001)
address constant ZONE_INBOX = 0x1c00000000000000000000000000000000000001;

// ZoneOutbox system contract address (0x1c00...0002)
address constant ZONE_OUTBOX = 0x1c00000000000000000000000000000000000002;

// ZoneConfig system contract address (0x1c00...0003)
address constant ZONE_CONFIG = 0x1c00000000000000000000000000000000000003;

/// @title IVerifier
/// @notice Interface for zone proof/attestation verification
interface IVerifier {

    /// @notice Verify a batch proof
    /// @dev The proof validates:
    ///      1. Valid state transition from prevBlockHash to nextBlockHash
    ///      2. Zone committed to tempoBlockNumber (via TempoState)
    ///      3. If anchorBlockNumber == tempoBlockNumber: zone's hash matches anchorBlockHash
    ///      4. If anchorBlockNumber > tempoBlockNumber: ancestry chain from tempoBlockNumber to anchorBlockNumber
    ///      5. ZoneOutbox.lastBatch().withdrawalBatchIndex == expectedWithdrawalBatchIndex
    ///      6. ZoneOutbox.lastBatch().withdrawalQueueHash matches withdrawalQueueTransition
    ///      7. Zone block beneficiary matches sequencer
    ///      8. Deposit processing is correct (validated via Tempo state read inside proof)
    /// @param tempoBlockNumber Block zone committed to (from TempoState)
    /// @param anchorBlockNumber Block whose hash is verified (tempoBlockNumber or recent block)
    /// @param anchorBlockHash Hash of anchorBlockNumber (from EIP-2935)
    /// @param expectedWithdrawalBatchIndex Expected batch index (portal.withdrawalBatchIndex + 1)
    /// @param sequencer Sequencer address (zone block beneficiary must match)
    /// @param blockTransition Zone block hash transition
    /// @param depositQueueTransition Deposit queue processing transition
    /// @param withdrawalQueueTransition Withdrawal queue hash for this batch
    /// @param verifierConfig Opaque payload for verifier (TEE attestation envelope, etc.)
    /// @param proof Validity proof or TEE attestation
    function verify(
        uint64 tempoBlockNumber,
        uint64 anchorBlockNumber,
        bytes32 anchorBlockHash,
        uint64 expectedWithdrawalBatchIndex,
        address sequencer,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
    )
        external
        view
        returns (bool);

}

/// @title IZoneFactory
/// @notice Interface for creating zones
interface IZoneFactory {

    struct CreateZoneParams {
        address token;
        address sequencer;
        address verifier;
        ZoneParams zoneParams;
    }

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

    error InvalidToken();
    error InvalidSequencer();
    error InvalidVerifier();

    function createZone(CreateZoneParams calldata params)
        external
        returns (uint64 zoneId, address portal);
    function zoneCount() external view returns (uint64);
    function zones(uint64 zoneId) external view returns (ZoneInfo memory);
    function isZonePortal(address portal) external view returns (bool);

}

/// @title IZonePortal
/// @notice Interface for zone portal on Tempo
interface IZonePortal {

    event DepositMade(
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed sender,
        address to,
        uint128 netAmount,
        uint128 fee,
        bytes32 memo
    );

    event BatchSubmitted(
        uint64 indexed withdrawalBatchIndex,
        bytes32 nextProcessedDepositQueueHash,
        bytes32 nextBlockHash,
        bytes32 withdrawalQueueHash
    );

    event WithdrawalProcessed(address indexed to, uint128 amount, bool callbackSuccess);

    event BounceBack(
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed fallbackRecipient,
        uint128 amount
    );

    event SequencerTransferStarted(
        address indexed currentSequencer, address indexed pendingSequencer
    );
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    /// @notice Emitted when an encrypted deposit is made (recipient/memo not revealed)
    event EncryptedDepositMade(
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed sender,
        uint128 amount,
        uint256 keyIndex,
        bytes32 ephemeralPubkeyX,
        uint8 ephemeralPubkeyYParity
    );

    /// @notice Emitted when sequencer updates their encryption key
    /// @param x The X coordinate of the new key
    /// @param yParity The Y coordinate parity (0x02 or 0x03)
    /// @param keyIndex The index of this key in the history array
    /// @param activationBlock The Tempo block when this key becomes active
    event SequencerEncryptionKeyUpdated(
        bytes32 x,
        uint8 yParity,
        uint256 keyIndex,
        uint64 activationBlock
    );
    event ZoneGasRateUpdated(uint128 zoneGasRate);

    error NotSequencer();
    error NotPendingSequencer();
    error InvalidProof();
    error InvalidTempoBlockNumber();
    error CallbackRejected();
    error EncryptionKeyExpired(uint256 keyIndex, uint64 activationBlock, uint64 supersededAtBlock);
    error InvalidEncryptionKeyIndex(uint256 keyIndex);
    error DepositTooSmall();

    /// @notice Fixed gas value for deposit fee calculation (100,000 gas)
    function FIXED_DEPOSIT_GAS() external view returns (uint64);

    function zoneId() external view returns (uint64);
    function token() external view returns (address);
    function messenger() external view returns (address);
    function sequencer() external view returns (address);
    function pendingSequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
    function zoneGasRate() external view returns (uint128);
    function verifier() external view returns (address);
    function withdrawalBatchIndex() external view returns (uint64);
    function blockHash() external view returns (bytes32);
    function currentDepositQueueHash() external view returns (bytes32);
    function lastSyncedTempoBlockNumber() external view returns (uint64);
    function withdrawalQueueHead() external view returns (uint256);
    function withdrawalQueueTail() external view returns (uint256);
    function withdrawalQueueMaxSize() external view returns (uint256);
    function withdrawalQueueSlot(uint256 slot) external view returns (bytes32);

    function genesisTempoBlockNumber() external view returns (uint64);

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    /// @param newSequencer The address that will become sequencer after accepting.
    function transferSequencer(address newSequencer) external;

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external;

    function setSequencerPubkey(bytes32 pubkey) external;

    /// @notice Get the sequencer's current encryption public key for encrypted deposits
    /// @return x The X coordinate of the secp256k1 public key
    /// @return yParity The Y coordinate parity (0x02 or 0x03)
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);

    /// @notice Set the sequencer's encryption public key. Only callable by sequencer.
    /// @dev Appends to key history. The new key becomes active at the current Tempo block.
    /// @param x The X coordinate of the secp256k1 public key
    /// @param yParity The Y coordinate parity (0x02 or 0x03)
    function setSequencerEncryptionKey(bytes32 x, uint8 yParity) external;

    /// @notice Get the number of encryption keys in the history
    /// @return The total count of keys (including current)
    function encryptionKeyCount() external view returns (uint256);

    /// @notice Get a historical encryption key by index
    /// @param index The index in the key history (0 = first key)
    /// @return entry The key entry with activation block
    function encryptionKeyAt(uint256 index) external view returns (EncryptionKeyEntry memory entry);

    /// @notice Get the encryption key that was active at a specific Tempo block
    /// @dev Binary search through key history to find the correct key
    /// @param tempoBlockNumber The Tempo block number to query
    /// @return x The X coordinate of the active key
    /// @return yParity The Y coordinate parity
    /// @return keyIndex The index of this key in history
    function encryptionKeyAtBlock(uint64 tempoBlockNumber)
        external view returns (bytes32 x, uint8 yParity, uint256 keyIndex);

    /// @notice Set zone gas rate. Only callable by sequencer.
    /// @param _zoneGasRate Gas token units per gas unit on the zone
    function setZoneGasRate(uint128 _zoneGasRate) external;

    /// @notice Calculate the fee for a deposit
    function calculateDepositFee() external view returns (uint128 fee);

    /// @notice Check if an encryption key is still valid for new deposits
    /// @dev A key is valid if it's the current key OR if it was superseded less than
    ///      ENCRYPTION_KEY_GRACE_PERIOD blocks ago
    /// @param keyIndex The key index to check
    /// @return valid True if the key can be used for new deposits
    /// @return expiresAtBlock Block number when this key expires (0 if current key, never expires)
    function isEncryptionKeyValid(uint256 keyIndex)
        external
        view
        returns (bool valid, uint64 expiresAtBlock);

    function deposit(address to, uint128 amount, bytes32 memo)
        external
        returns (bytes32 newCurrentDepositQueueHash);

    /// @notice Deposit with encrypted recipient and memo
    /// @dev The encrypted payload contains (to, memo) encrypted to the sequencer's key
    ///      at the specified keyIndex. The user must specify which key they encrypted to,
    ///      ensuring correct decryption even if the key rotates before inclusion.
    /// @param amount Amount to deposit
    /// @param keyIndex Index of the encryption key used (from encryptionKeyAt)
    /// @param encrypted The encrypted payload (recipient and memo)
    /// @return newCurrentDepositQueueHash The new deposit queue hash
    function depositEncrypted(
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata encrypted
    ) external returns (bytes32 newCurrentDepositQueueHash);
    function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external;
    function submitBatch(
        uint64 tempoBlockNumber,
        uint64 recentTempoBlockNumber,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
    )
        external;

}

/// @title IZoneMessenger
/// @notice Interface for zone messenger on Tempo (handles withdrawal callbacks)
interface IZoneMessenger {

    /// @notice Returns the zone's portal address
    function portal() external view returns (address);

    /// @notice Returns the zone token address
    function token() external view returns (address);

    /// @notice Returns the L2 sender during callback execution
    /// @dev Reverts if not in a callback context
    function xDomainMessageSender() external view returns (address);

    /// @notice Relay a withdrawal message. Only callable by the portal.
    /// @dev Transfers tokens from portal to target via transferFrom, then executes callback.
    ///      If callback reverts, the entire call reverts (including the transfer).
    /// @param sender The L2 origin address
    /// @param target The Tempo recipient
    /// @param amount Tokens to transfer from portal to target
    /// @param gasLimit Max gas for the callback
    /// @param data Calldata for the target
    function relayMessage(
        address sender,
        address target,
        uint128 amount,
        uint64 gasLimit,
        bytes calldata data
    )
        external;

}

/// @title IWithdrawalReceiver
/// @notice Interface for contracts that receive withdrawals with callbacks
interface IWithdrawalReceiver {

    function onWithdrawalReceived(
        address sender,
        uint128 amount,
        bytes calldata callbackData
    )
        external
        returns (bytes4);

}

/// @notice Withdrawal batch parameters stored in state for proof access
/// @dev Written to storage on each finalizeWithdrawalBatch() call so proofs can read from state root
///      instead of parsing event logs (which are expensive and hard to prove)
struct LastBatch {
    bytes32 withdrawalQueueHash;
    uint64 withdrawalBatchIndex;
}

/// @title ITempoState
/// @notice Interface for zone-side Tempo state verification predeploy
/// @dev Deployed at 0x1c00000000000000000000000000000000000000
///      System-only contract. Only ZoneInbox can call finalizeTempo().
///      Only ZoneInbox, ZoneOutbox, and ZoneConfig can call readTempoStorageSlot(s).
interface ITempoState {

    event TempoBlockFinalized(
        bytes32 indexed blockHash, uint64 indexed blockNumber, bytes32 stateRoot
    );

    error InvalidParentHash();
    error InvalidBlockNumber();
    error InvalidRlpData();
    error OnlyZoneInbox();

    /// @notice Current finalized Tempo block hash (keccak256 of RLP-encoded header)
    function tempoBlockHash() external view returns (bytes32);

    // Tempo wrapper fields
    function generalGasLimit() external view returns (uint64);
    function sharedGasLimit() external view returns (uint64);

    // Inner Ethereum header fields
    function tempoParentHash() external view returns (bytes32);
    function tempoBeneficiary() external view returns (address);
    function tempoStateRoot() external view returns (bytes32);
    function tempoTransactionsRoot() external view returns (bytes32);
    function tempoReceiptsRoot() external view returns (bytes32);
    function tempoBlockNumber() external view returns (uint64);
    function tempoGasLimit() external view returns (uint64);
    function tempoGasUsed() external view returns (uint64);
    function tempoTimestamp() external view returns (uint64);
    function tempoTimestampMillis() external view returns (uint64);
    function tempoPrevRandao() external view returns (bytes32);

    /// @notice Finalize a Tempo block header. Only callable by ZoneInbox.
    /// @dev Validates chain continuity (parent hash must match, number must be +1).
    ///      Called by ZoneInbox.advanceTempo(). Executor enforces ZoneInbox-only access.
    /// @param header RLP-encoded Tempo header
    function finalizeTempo(bytes calldata header) external;

    /// @notice Read a storage slot from a Tempo contract
    function readTempoStorageSlot(address account, bytes32 slot) external view returns (bytes32);

    /// @notice Read multiple storage slots from a Tempo contract
    function readTempoStorageSlots(
        address account,
        bytes32[] calldata slots
    )
        external
        view
        returns (bytes32[] memory);

}

/// @title IZoneInbox
/// @notice Interface for zone-side system contract that advances Tempo state and processes deposits
interface IZoneInbox {

    event TempoAdvanced(
        bytes32 indexed tempoBlockHash,
        uint64 indexed tempoBlockNumber,
        uint256 depositsProcessed,
        bytes32 newProcessedDepositQueueHash
    );

    event DepositProcessed(
        bytes32 indexed depositHash,
        address indexed sender,
        address indexed to,
        uint128 amount,
        bytes32 memo
    );

    /// @notice Emitted when an encrypted deposit is processed (decrypted and credited)
    event EncryptedDepositProcessed(
        bytes32 indexed depositHash,
        address indexed sender,
        address indexed to,      // Revealed after decryption
        uint128 amount,
        bytes32 memo             // Revealed after decryption
    );
    error OnlySequencer();
    error InvalidDepositQueueHash();
    error MissingDecryptionData();
    error ExtraDecryptionData();
    error InvalidDecryption();

    /// @notice Zone configuration (reads sequencer from L1)
    function config() external view returns (IZoneConfig);

    /// @notice The Tempo portal address (for reading deposit queue hash)
    function tempoPortal() external view returns (address);

    /// @notice The TempoState predeploy address
    function tempoState() external view returns (ITempoState);

    /// @notice The zone token (TIP-20 at same address as Tempo)
    function gasToken() external view returns (IZoneToken);

    /// @notice The zone's last processed deposit queue hash
    function processedDepositQueueHash() external view returns (bytes32);

    /// @notice Advance Tempo state and process deposits in a single sequencer-only call.
    /// @dev This is the main entry point for the sequencer at block start.
    ///      1. Advances the zone's view of Tempo by processing the header
    ///      2. Processes deposits from the unified queue (regular and encrypted)
    ///      3. Validates the resulting hash against Tempo's currentDepositQueueHash
    ///
    ///      For encrypted deposits, the sequencer provides DecryptionData with the
    ///      decrypted (to, memo) values. The proof/TEE validates correctness.
    ///
    /// @param header RLP-encoded Tempo block header
    /// @param deposits Array of queued deposits to process (oldest first, must be contiguous)
    /// @param decryptions Decryption data for encrypted deposits (1:1 with encrypted deposits, in order)
    function advanceTempo(
        bytes calldata header,
        QueuedDeposit[] calldata deposits,
        DecryptionData[] calldata decryptions
    ) external;
}

/// @title IZoneOutbox
/// @notice Interface for zone outbox on the zone
interface IZoneOutbox {

    /// @notice Maximum callback data size (1KB)
    function MAX_CALLBACK_DATA_SIZE() external view returns (uint256);

    /// @notice Base gas cost for processing a withdrawal on Tempo (excluding callback)
    function WITHDRAWAL_BASE_GAS() external view returns (uint64);

    event WithdrawalRequested(
        uint64 indexed withdrawalIndex,
        address indexed sender,
        address to,
        uint128 amount,
        uint128 fee,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes data
    );

    event TempoGasRateUpdated(uint128 tempoGasRate);

    /// @notice Emitted when sequencer finalizes a batch at end of block
    /// @dev Kept for observability. Proof reads from lastBatch storage instead.
    event BatchFinalized(bytes32 indexed withdrawalQueueHash, uint64 withdrawalBatchIndex);

    /// @notice Zone configuration (reads sequencer from L1)
    function config() external view returns (IZoneConfig);

    /// @notice The zone token (same as Tempo portal's token)
    function gasToken() external view returns (IZoneToken);

    /// @notice Tempo gas rate (gas token units per gas unit on Tempo)
    /// @dev Fee = (WITHDRAWAL_BASE_GAS + gasLimit) * tempoGasRate
    function tempoGasRate() external view returns (uint128);

    /// @notice Next withdrawal index (monotonically increasing)
    function nextWithdrawalIndex() external view returns (uint64);

    /// @notice Current withdrawal batch index (monotonically increasing)
    function withdrawalBatchIndex() external view returns (uint64);

    /// @notice Last finalized batch parameters (for proof access via state root)
    function lastBatch() external view returns (LastBatch memory);

    /// @notice Number of pending withdrawals
    function pendingWithdrawalsCount() external view returns (uint256);

    /// @notice Set Tempo gas rate. Only callable by sequencer.
    /// @dev Sequencer publishes this rate and takes the risk on Tempo gas price fluctuations.
    /// @param _tempoGasRate Gas token units per gas unit on Tempo
    function setTempoGasRate(uint128 _tempoGasRate) external;

    /// @notice Calculate the fee for a withdrawal with the given gasLimit
    /// @dev Fee = (WITHDRAWAL_BASE_GAS + gasLimit) * tempoGasRate
    function calculateWithdrawalFee(uint64 gasLimit) external view returns (uint128);

    /// @notice Request a withdrawal from the zone back to Tempo
    /// @dev Caller must approve outbox to spend amount + fee
    function requestWithdrawal(
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes calldata data
    )
        external;

    /// @notice Finalize batch at end of block - build withdrawal hash and write to state
    /// @dev Only callable by sequencer. Required per batch (count may be 0).
    ///      Writes withdrawal batch parameters to lastBatch storage for proof access.
    /// @param count Max number of withdrawals to process
    /// @return withdrawalQueueHash The hash chain (0 if no withdrawals)
    function finalizeWithdrawalBatch(uint256 count) external returns (bytes32 withdrawalQueueHash);

}

/// @title IZoneConfig
/// @notice Interface for zone configuration and L1 state access
/// @dev System contract predeploy at 0x1c00000000000000000000000000000000000003
///      Provides centralized access to zone metadata and reads sequencer from L1.
interface IZoneConfig {

    error NotSequencer();

    /// @notice Zone token address (TIP-20 at same address as Tempo)
    function zoneToken() external view returns (address);

    /// @notice L1 ZonePortal address
    function tempoPortal() external view returns (address);

    /// @notice TempoState predeploy for L1 reads
    function tempoState() external view returns (ITempoState);

    /// @notice Get current sequencer by reading from L1 ZonePortal
    /// @dev Reads from finalized Tempo state. L1 is single source of truth.
    function sequencer() external view returns (address);

    /// @notice Get pending sequencer by reading from L1 ZonePortal
    function pendingSequencer() external view returns (address);

    /// @notice Get sequencer's encryption public key by reading from L1 ZonePortal
    /// @dev Used for encrypted deposits (ECIES).
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);

    /// @notice Check if an address is the current sequencer
    function isSequencer(address account) external view returns (bool);

    /// @notice Get zone token as IZoneToken interface
    function getZoneToken() external view returns (IZoneToken);

}
