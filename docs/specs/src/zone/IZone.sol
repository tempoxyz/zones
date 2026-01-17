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

/// @notice Deposit queue message kinds (Tempo -> zone)
enum DepositQueueMessageKind {
    Deposit,
    L1Sync
}

/// @notice L1 sync message payload (Tempo -> zone)
struct L1Sync {
    bytes32 l1BlockHash;
    uint64 l1BlockNumber;
    uint64 l1Timestamp;
}

/// @notice Deposit queue message wrapper (Tempo -> zone)
struct DepositQueueMessage {
    DepositQueueMessageKind kind;
    bytes data; // abi.encode(Deposit) or abi.encode(L1Sync)
}

struct Deposit {
    // L1 block info (zone receives L1 state through deposit queue messages)
    bytes32 l1BlockHash;
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
    uint64 gasLimit;            // max gas for IExitReceiver callback (0 = no callback)
    address fallbackRecipient;  // zone address for bounce-back if call fails
    bytes data;                 // calldata for IExitReceiver (if gasLimit > 0)
}

/// @title IVerifier
/// @notice Interface for zone proof/attestation verification
interface IVerifier {
    function verify(
        // Deposit queue
        bytes32 prevProcessedDepositQueueHash,     // where proof starts (from portal state)
        bytes32 prevPendingDepositQueueHash,       // stable target ceiling (from portal state)
        bytes32 nextProcessedDepositQueueHash,     // where zone processed up to (from batch)

        // Zone state transition
        bytes32 prevStateRoot,
        bytes32 nextStateRoot,

        // Withdrawal queue updates (proof outputs)
        bytes32 prevPendingWithdrawalQueueHash,        // what proof assumed pending queue was
        bytes32 nextPendingWithdrawalQueueHashIfFull,  // pending queue after append if no swap
        bytes32 nextPendingWithdrawalQueueHashIfEmpty, // pending queue after append if swap occurred

        // Opaque verifier payload (e.g., attestation envelope, domain separation data)
        bytes calldata verifierData,

        // Proof
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
        uint64 indexed zoneId,
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed sender,
        address to,
        uint128 amount,
        bytes32 memo,
        bytes32 l1BlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    event L1SyncAppended(
        uint64 indexed zoneId,
        bytes32 indexed newCurrentDepositQueueHash,
        bytes32 l1BlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    event BatchSubmitted(
        uint64 indexed zoneId,
        uint64 indexed batchIndex,
        bytes32 prevProcessedDepositQueueHash,           // pre-state input to verifier
        bytes32 prevPendingDepositQueueHash,             // pre-state input to verifier
        bytes32 nextProcessedDepositQueueHash,           // verifier input/output
        bytes32 prevStateRoot,                           // pre-state input to verifier
        bytes32 nextStateRoot,                           // verifier output
        bytes32 prevPendingWithdrawalQueueHash,          // verifier input
        bytes32 nextPendingWithdrawalQueueHashIfFull,    // verifier output path 1
        bytes32 nextPendingWithdrawalQueueHashIfEmpty    // verifier output path 2
    );

    event WithdrawalProcessed(
        uint64 indexed zoneId,
        address indexed to,
        uint128 amount,
        bool callbackSuccess
    );

    event BounceBack(
        uint64 indexed zoneId,
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed fallbackRecipient,
        uint128 amount
    );

    error NotSequencer();
    error InvalidProof();
    error NoWithdrawals();
    error InvalidWithdrawal();
    error UnexpectedPendingWithdrawalQueueHash();
    error CallbackRejected();

    function zoneId() external view returns (uint64);
    function token() external view returns (address);
    function sequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
    function verifier() external view returns (address);
    function batchIndex() external view returns (uint64);
    function stateRoot() external view returns (bytes32);
    function processedDepositQueueHash() external view returns (bytes32);
    function pendingDepositQueueHash() external view returns (bytes32);
    function currentDepositQueueHash() external view returns (bytes32);
    function activeWithdrawalQueueHash() external view returns (bytes32);
    function pendingWithdrawalQueueHash() external view returns (bytes32);

    function setSequencerPubkey(bytes32 pubkey) external;
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositQueueHash);
    function syncL1() external returns (bytes32 newCurrentDepositQueueHash);
    function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external;
    function submitBatch(
        bytes32 nextProcessedDepositQueueHash,
        bytes32 nextStateRoot,
        bytes32 prevPendingWithdrawalQueueHash,
        bytes32 nextPendingWithdrawalQueueHashIfFull,
        bytes32 nextPendingWithdrawalQueueHashIfEmpty,
        bytes calldata verifierData,
        bytes calldata proof
    ) external;
}

/// @title IExitReceiver
/// @notice Interface for contracts that receive exits with callbacks
interface IExitReceiver {
    function onExitReceived(
        address sender,
        uint128 amount,
        bytes calldata data
    ) external returns (bytes4);
}
