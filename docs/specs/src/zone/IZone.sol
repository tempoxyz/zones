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
    bytes32 prevProcessedHash;     // where proof starts (verified against zone state)
    bytes32 nextProcessedHash;     // where zone processed up to (proof output)
}

/// @notice Withdrawal queue transition for batch proofs
/// @dev Each batch gets its own slot in an unbounded buffer.
///      The withdrawalQueueHash is the hash chain of withdrawals for this batch.
struct WithdrawalQueueTransition {
    bytes32 withdrawalQueueHash;  // hash chain of withdrawals for this batch (0 if none)
}

struct Deposit {
    address sender;
    address to;
    uint128 amount;
    bytes32 memo;
}

struct Withdrawal {
    address sender;             // who initiated the withdrawal on the zone
    address to;                 // Tempo recipient
    uint128 amount;             // amount to send to recipient (excludes fee)
    uint128 fee;                // processing fee for sequencer (calculated at request time)
    bytes32 memo;               // user-provided context
    uint64 gasLimit;            // max gas for IWithdrawalReceiver callback (0 = no callback)
    address fallbackRecipient;  // zone address for bounce-back if call fails
    bytes callbackData;         // calldata for IWithdrawalReceiver (if gasLimit > 0)
}

/// @notice L1 state access entry for replay and reconstruction
/// @dev Records a single L1 storage slot read during zone block execution
struct L1StateAccessEntry {
    address account;  // L1 contract address
    bytes32 slot;     // Storage slot
    bytes32 value;    // Storage value at the time of access
}

/// @notice L1 state access log for a zone block
/// @dev Includes all L1 state reads from genesis, enabling full replay/reconstruction
///      Sequencer commits to this log in the batch proof
struct L1StateAccessLog {
    L1StateAccessEntry[] accesses;  // All L1 state accesses in this block
}

/// @title IVerifier
/// @notice Interface for zone proof/attestation verification
interface IVerifier {
    /// @notice Verify a batch proof
    /// @dev The proof validates:
    ///      1. Valid state transition from prevBlockHash to nextBlockHash
    ///      2. Zone's TempoState.tempoBlockHash() matches tempoBlockHash for tempoBlockNumber
    ///      3. ZoneOutbox.lastBatch().withdrawalBatchIndex == expectedWithdrawalBatchIndex
    ///      4. ZoneOutbox.lastBatch().withdrawalQueueHash matches withdrawalQueueTransition
    ///      5. Zone block beneficiary matches sequencer
    ///      6. Deposit processing is correct (validated via Tempo state read inside proof)
    /// @param tempoBlockNumber The Tempo block number for EIP-2935 lookup
    /// @param tempoBlockHash The Tempo block hash (from EIP-2935)
    /// @param expectedWithdrawalBatchIndex The expected batch index (portal.withdrawalBatchIndex + 1)
    /// @param sequencer The registered sequencer address (zone block beneficiary must match)
    /// @param blockTransition The zone block hash transition
    /// @param depositQueueTransition The deposit queue processing transition
    /// @param withdrawalQueueTransition The withdrawal queue hash for this batch
    /// @param verifierConfig Opaque payload for verifier (TEE attestation envelope, etc.)
    /// @param proof The validity proof or TEE attestation
    function verify(
        uint64 tempoBlockNumber,
        bytes32 tempoBlockHash,
        uint64 expectedWithdrawalBatchIndex,
        address sequencer,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
    ) external view returns (bool);
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

    function createZone(CreateZoneParams calldata params) external returns (uint64 zoneId, address portal);
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
        uint128 amount,
        bytes32 memo
    );

    event BatchSubmitted(
        uint64 indexed withdrawalBatchIndex,
        bytes32 nextProcessedDepositQueueHash,
        bytes32 nextBlockHash,
        bytes32 withdrawalQueueHash
    );

    event WithdrawalProcessed(
        address indexed to,
        uint128 amount,
        bool callbackSuccess
    );

    event BounceBack(
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed fallbackRecipient,
        uint128 amount
    );

    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    error NotSequencer();
    error NotPendingSequencer();
    error InvalidProof();
    error InvalidTempoBlockNumber();
    error CallbackRejected();

    function zoneId() external view returns (uint64);
    function token() external view returns (address);
    function messenger() external view returns (address);
    function sequencer() external view returns (address);
    function pendingSequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
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
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositQueueHash);
    function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external;
    function submitBatch(
        uint64 tempoBlockNumber,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
    ) external;
}

/// @title IZoneMessenger
/// @notice Interface for zone messenger on Tempo (handles withdrawal callbacks)
interface IZoneMessenger {
    /// @notice Returns the zone's portal address
    function portal() external view returns (address);

    /// @notice Returns the gas token address
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
    ) external;
}

/// @title IWithdrawalReceiver
/// @notice Interface for contracts that receive withdrawals with callbacks
interface IWithdrawalReceiver {
    function onWithdrawalReceived(
        address sender,
        uint128 amount,
        bytes calldata callbackData
    ) external returns (bytes4);
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
interface ITempoState {
    event TempoBlockFinalized(bytes32 indexed blockHash, uint64 indexed blockNumber, bytes32 stateRoot);
    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    error OnlySequencer();
    error NotPendingSequencer();
    error InvalidParentHash();
    error InvalidBlockNumber();
    error InvalidRlpData();

    /// @notice Current sequencer address
    function sequencer() external view returns (address);

    /// @notice Pending sequencer for two-step transfer
    function pendingSequencer() external view returns (address);

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

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    function transferSequencer(address newSequencer) external;

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external;

    /// @notice Finalize a Tempo block header. Only callable by sequencer.
    /// @dev Validates chain continuity (parent hash must match, number must be +1)
    /// @param header RLP-encoded Tempo header
    function finalizeTempo(bytes calldata header) external;

    /// @notice Read a storage slot from a Tempo contract
    /// @dev The slot must be subscribed via L1StateSubscriptionManager.
    ///      Reads from the sequencer's synced L1 state cache.
    ///      Logs the access to the current block's L1StateAccessLog for replay.
    /// @param account The Tempo contract address
    /// @param slot The storage slot to read
    /// @return value The storage value from synced L1 state
    function readTempoStorageSlot(address account, bytes32 slot) external view returns (bytes32);

    /// @notice Read multiple storage slots from a Tempo contract
    /// @dev All slots must be subscribed via L1StateSubscriptionManager.
    ///      Reads from the sequencer's synced L1 state cache.
    ///      Logs all accesses to the current block's L1StateAccessLog for replay.
    /// @param account The Tempo contract address
    /// @param slots The storage slots to read
    /// @return values The storage values from synced L1 state
    function readTempoStorageSlots(address account, bytes32[] calldata slots) external view returns (bytes32[] memory);
}

/// @title IZoneConfig
/// @notice Interface for central zone configuration and L1 state references
/// @dev Deployed at 0x1c00000000000000000000000000000000000002
///      Provides single source of truth for zone metadata and sequencer state (read from L1).
interface IZoneConfig {
    error NotSequencer();

    /// @notice Zone token address (same on L1 and L2)
    function zoneToken() external view returns (address);

    /// @notice L1 ZonePortal address
    function l1Portal() external view returns (address);

    /// @notice L1 TIP-403 registry address
    function l1TIP403Registry() external view returns (address);

    /// @notice TempoState predeploy for L1 reads
    function tempoState() external view returns (ITempoState);

    /// @notice L1StateSubscriptionManager for subscription checks
    function subscriptionManager() external view returns (IL1StateSubscriptionManager);

    /// @notice Get current sequencer by reading from L1 ZonePortal
    function sequencer() external view returns (address);

    /// @notice Get pending sequencer by reading from L1 ZonePortal
    function pendingSequencer() external view returns (address);

    /// @notice Check if an address is the current sequencer
    function isSequencer(address account) external view returns (bool);

    /// @notice Get zone token as IZoneToken interface
    function getZoneToken() external view returns (IZoneToken);
}

/// @title IL1StateSubscriptionManager
/// @notice Interface for L1 state subscription management on the zone
/// @dev Deployed at 0x1c00000000000000000000000000000000000001
///      Sequencer is read from L1 via ZoneConfig (not stored locally).
interface IL1StateSubscriptionManager {
    event SubscriptionCreated(address indexed account, bytes32 indexed slot, uint64 expiryTimestamp);
    event SubscriptionExtended(address indexed account, bytes32 indexed slot, uint64 newExpiryTimestamp);
    event DailyFeeUpdated(uint128 newFee);

    error OnlySequencer();
    error SubscriptionExpired();
    error InsufficientPayment();
    error PermanentSubscription();

    /// @notice The zone token used to pay subscription fees
    function zoneToken() external view returns (IZoneToken);

    /// @notice Current sequencer address
    function sequencer() external view returns (address);

    /// @notice Pending sequencer for two-step transfer
    function pendingSequencer() external view returns (address);

    /// @notice Daily subscription fee per (account, slot) pair
    function dailySubscriptionFee() external view returns (uint128);

    /// @notice Get subscription expiry timestamp
    /// @param account The L1 contract address
    /// @param slot The storage slot
    /// @return expiry Expiry timestamp (0 if never subscribed, max uint64 if permanent)
    function getSubscriptionExpiry(address account, bytes32 slot) external view returns (uint64 expiry);

    /// @notice TIP-403 registry address on L1
    function tip403Registry() external view returns (address);

    /// @notice Zone token address on L1
    function l1ZoneToken() external view returns (address);

    /// @notice Set daily subscription fee. Only callable by sequencer (read from L1 via ZoneConfig).
    function setDailyFee(uint128 newFee) external;

    /// @notice Subscribe to an L1 state slot for N days
    /// @dev Transfers zone tokens equal to dailySubscriptionFee * days to sequencer
    /// @param account The L1 contract address
    /// @param slot The storage slot
    /// @param days Number of days to subscribe (1-3650, ~10 years max)
    function subscribe(address account, bytes32 slot, uint16 days) external;

    /// @notice Auto-subscribe to TIP-403 policy state for the zone token
    /// @dev Called by sequencer after reading transferPolicyId from L1
    /// @param transferPolicyId The zone token's transfer policy ID from L1
    function autoSubscribePolicyState(uint256 transferPolicyId) external;

    /// @notice Check if a subscription is active
    /// @param account The L1 contract address
    /// @param slot The storage slot
    /// @return active True if subscription is active (not expired)
    function isSubscribed(address account, bytes32 slot) external view returns (bool active);
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

    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    error OnlySequencer();
    error NotPendingSequencer();
    error InvalidDepositQueueHash();

    /// @notice The Tempo portal address (for reading deposit queue hash)
    function tempoPortal() external view returns (address);

    /// @notice The TempoState predeploy address
    function tempoState() external view returns (ITempoState);

    /// @notice The zone token (TIP-20 at same address as Tempo)
    function gasToken() external view returns (IZoneToken);

    /// @notice Current sequencer address
    function sequencer() external view returns (address);

    /// @notice Pending sequencer for two-step transfer
    function pendingSequencer() external view returns (address);

    /// @notice The zone's last processed deposit queue hash
    function processedDepositQueueHash() external view returns (bytes32);

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    function transferSequencer(address newSequencer) external;

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external;

    /// @notice Advance Tempo state and process deposits in a single system transaction.
    /// @dev This is the main entry point for the sequencer's system transaction.
    ///      1. Advances the zone's view of Tempo by processing the header
    ///      2. Processes deposits from the deposit queue
    ///      3. Validates the resulting hash against Tempo's currentDepositQueueHash
    /// @param header RLP-encoded Tempo block header
    /// @param deposits Array of deposits to process (oldest first, must be contiguous from processedDepositQueueHash)
    function advanceTempo(bytes calldata header, Deposit[] calldata deposits) external;
}

/// @title IZoneOutbox
/// @notice Interface for zone outbox on the zone
interface IZoneOutbox {
    /// @notice Maximum callback data size (1KB)
    function MAX_CALLBACK_DATA_SIZE() external view returns (uint256);

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

    event WithdrawalFeesUpdated(uint128 baseFee, uint128 gasFeeRate);

    /// @notice Emitted when sequencer finalizes a batch at end of block
    /// @dev Kept for observability. Proof reads from lastBatch storage instead.
    event BatchFinalized(
        bytes32 indexed withdrawalQueueHash,
        uint64 withdrawalBatchIndex
    );

    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    /// @notice The zone token (same as Tempo portal's token)
    function gasToken() external view returns (IZoneToken);

    /// @notice Current sequencer address
    function sequencer() external view returns (address);

    /// @notice Pending sequencer for two-step transfer
    function pendingSequencer() external view returns (address);

    /// @notice Base fee for withdrawal processing
    function withdrawalBaseFee() external view returns (uint128);

    /// @notice Fee per unit of gasLimit
    function withdrawalGasFeeRate() external view returns (uint128);

    /// @notice Next withdrawal index (monotonically increasing)
    function nextWithdrawalIndex() external view returns (uint64);

    /// @notice Current withdrawal batch index (monotonically increasing)
    function withdrawalBatchIndex() external view returns (uint64);

    /// @notice Last finalized batch parameters (for proof access via state root)
    function lastBatch() external view returns (LastBatch memory);

    /// @notice Number of pending withdrawals
    function pendingWithdrawalsCount() external view returns (uint256);

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    function transferSequencer(address newSequencer) external;

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external;

    /// @notice Set withdrawal fee parameters. Only callable by sequencer.
    function setWithdrawalFees(uint128 baseFee, uint128 gasFeeRate) external;

    /// @notice Calculate the fee for a withdrawal with the given gasLimit
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
    ) external;

    /// @notice Finalize batch at end of block - build withdrawal hash and write to state
    /// @dev Only callable by sequencer as system transaction. Required per batch (count may be 0).
    ///      Writes withdrawal batch parameters to lastBatch storage for proof access.
    /// @param count Max number of withdrawals to process
    /// @return withdrawalQueueHash The hash chain (0 if no withdrawals)
    function finalizeWithdrawalBatch(uint256 count) external returns (bytes32 withdrawalQueueHash);
}
