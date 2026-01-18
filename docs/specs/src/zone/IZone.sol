// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @notice Common types for the Zone protocol
struct ZoneInfo {
    uint64 zoneId;
    address portal;
    address token;
    address sequencer;
    address verifier;
    bytes32 genesisStateRoot;
}

/// @notice State transition for zone batch proofs
struct StateTransition {
    bytes32 prevStateRoot;
    bytes32 nextStateRoot;
}

/// @notice Deposit queue transition inputs/outputs for batch proofs
struct DepositQueueTransition {
    bytes32 prevSnapshotHash;      // stable target ceiling
    bytes32 prevProcessedHash;     // where proof starts
    bytes32 nextProcessedHash;     // where zone processed up to
}

/// @notice Withdrawal queue transition inputs/outputs for batch proofs
struct WithdrawalQueueTransition {
    bytes32 prevPendingHash;           // what proof assumed pending queue was
    bytes32 nextPendingHashIfNoSwap;   // pending queue after append if no swap occurred
    bytes32 nextPendingHashIfSwapped;  // pending queue after append if swap occurred
}

struct Deposit {
    // L1 block info (zone receives L1 state through deposit queue messages)
    bytes32 l1ParentBlockHash;
    uint64 l1BlockNumber;
    uint64 l1Timestamp;
    // Deposit data
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
        StateTransition calldata stateTransition,
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
        bytes32 genesisStateRoot;
    }

    event ZoneCreated(
        uint64 indexed zoneId,
        address indexed portal,
        address indexed token,
        address sequencer,
        address verifier,
        bytes32 genesisStateRoot
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
        bytes32 memo,
        bytes32 l1ParentBlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    event BatchSubmitted(
        uint64 indexed batchIndex,
        bytes32 nextProcessedDepositQueueHash,
        bytes32 nextStateRoot,
        bytes32 nextPendingWithdrawalQueueHash
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
    error CallbackRejected();

    function zoneId() external view returns (uint64);
    function token() external view returns (address);
    function sequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
    function verifier() external view returns (address);
    function batchIndex() external view returns (uint64);
    function stateRoot() external view returns (bytes32);
    function processedDepositQueueHash() external view returns (bytes32);
    function snapshotDepositQueueHash() external view returns (bytes32);
    function currentDepositQueueHash() external view returns (bytes32);
    function activeWithdrawalQueueHash() external view returns (bytes32);
    function pendingWithdrawalQueueHash() external view returns (bytes32);

    function setSequencerPubkey(bytes32 pubkey) external;
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositQueueHash);
    function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external;
    function submitBatch(
        StateTransition calldata stateTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
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
