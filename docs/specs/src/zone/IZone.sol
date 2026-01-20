// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

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

/// @notice Block transition for zone batch proofs
/// @dev Uses block hash instead of state root to commit to full block structure
///      (includes state root, transactions root, receipts root, etc.)
struct BlockTransition {
    bytes32 prevBlockHash;
    bytes32 nextBlockHash;
}

/// @notice Deposit queue transition inputs/outputs for batch proofs
/// @dev The proof reads currentDepositQueueHash from Tempo state to validate
///      that nextProcessedHash is a valid ancestor. No ceiling needed on-chain.
struct DepositQueueTransition {
    bytes32 prevProcessedHash;     // where proof starts (verified against on-chain state)
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
    uint128 amount;
    bytes32 memo;               // user-provided context
    uint64 gasLimit;            // max gas for IWithdrawalReceiver callback (0 = no callback)
    address fallbackRecipient;  // zone address for bounce-back if call fails
    bytes callbackData;         // calldata for IWithdrawalReceiver (if gasLimit > 0)
}

/// @title IVerifier
/// @notice Interface for zone proof/attestation verification
interface IVerifier {
    function verify(
        bytes32 tempoBlockHash,
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
        bytes32 genesisBlockHash;
        bytes32 genesisTempoBlockHash;
        uint64 genesisTempoBlockNumber;
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
        uint64 indexed batchIndex,
        uint64 tempoBlockNumber,
        bytes32 nextProcessedDepositQueueHash,
        bytes32 nextBlockHash,
        uint256 withdrawalQueueTail,
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

    error NotSequencer();
    error InvalidProof();
    error InvalidTempoBlockNumber();
    error CallbackRejected();

    function zoneId() external view returns (uint64);
    function token() external view returns (address);
    function messenger() external view returns (address);
    function sequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
    function verifier() external view returns (address);
    function batchIndex() external view returns (uint64);
    function blockHash() external view returns (bytes32);
    function currentDepositQueueHash() external view returns (bytes32);
    function lastSyncedTempoBlockNumber() external view returns (uint64);
    function withdrawalQueueHead() external view returns (uint256);
    function withdrawalQueueTail() external view returns (uint256);
    function withdrawalQueueMaxSize() external view returns (uint256);
    function withdrawalQueueSlot(uint256 slot) external view returns (bytes32);

    function genesisTempoBlockNumber() external view returns (uint64);

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
    /// @param target The L1 recipient
    /// @param amount Tokens to transfer from portal to target
    /// @param gasLimit Max gas for the callback
    /// @param data Calldata for the target
    /// @return success Whether the callback was successful
    function relayMessage(
        address sender,
        address target,
        uint128 amount,
        uint64 gasLimit,
        bytes calldata data
    ) external returns (bool success);
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

/// @title IZoneOutbox
/// @notice Interface for zone outbox on the zone
interface IZoneOutbox {
    /// @notice Emitted when sequencer finalizes a batch at end of block
    /// @dev Contains all proof inputs except state/block hash (computed after this tx)
    event BatchFinalized(
        bytes32 indexed withdrawalQueueHash,
        uint64 tempoBlockNumber,
        bytes32 tempoBlockHash,
        uint64 batchIndex,
        uint64 batchBlockNumber
    );

    /// @notice Request a withdrawal from the zone back to Tempo
    function requestWithdrawal(
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes calldata data
    ) external;

    /// @notice Finalize batch at end of block - build withdrawal hash and emit proof inputs
    /// @dev Only callable by sequencer as system transaction. Optional per block.
    /// @param count Max number of withdrawals to process
    /// @return withdrawalQueueHash The hash chain (0 if no withdrawals)
    function finalizeBatch(uint256 count) external returns (bytes32 withdrawalQueueHash);

    /// @notice Number of pending withdrawals
    function pendingWithdrawalsCount() external view returns (uint256);

    /// @notice Current batch index (monotonically increasing)
    function batchIndex() external view returns (uint64);
}
