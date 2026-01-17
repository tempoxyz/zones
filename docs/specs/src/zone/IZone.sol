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

struct BatchCommitment {
    bytes32 newProcessedDepositsHash;
    bytes32 newStateRoot;
}

struct Deposit {
    // L1 block info (zone receives L1 state through deposits)
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
    uint64 gasLimit;            // max gas for IExitReceiver callback (0 = no callback)
    address fallbackRecipient;  // zone address for bounce-back if call fails
    bytes data;                 // calldata for IExitReceiver (if gasLimit > 0)
}

/// @title IVerifier
/// @notice Interface for zone proof/attestation verification
interface IVerifier {
    function verify(
        // Deposit chain
        bytes32 processedDepositsHash,     // where proof starts (from portal state)
        bytes32 pendingDepositsHash,       // stable target ceiling (from portal state)
        bytes32 newProcessedDepositsHash,  // where zone processed up to (from batch)

        // Zone state transition
        bytes32 prevStateRoot,
        bytes32 newStateRoot,

        // Withdrawal queue updates (proof outputs)
        bytes32 expectedQueue2,       // what proof assumed queue2 was
        bytes32 updatedQueue2,        // queue2 with new withdrawals added to innermost
        bytes32 newWithdrawalsOnly,   // new withdrawals only (only used if queue2 was empty)

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
        bytes32 indexed newCurrentDepositsHash,
        address indexed sender,
        address to,
        uint128 amount,
        bytes32 memo,
        bytes32 l1BlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    event BatchSubmitted(
        uint64 indexed zoneId,
        uint64 indexed batchIndex,
        bytes32 processedDepositsHash,      // pre-state input to verifier
        bytes32 pendingDepositsHash,        // pre-state input to verifier
        bytes32 newProcessedDepositsHash,   // verifier input/output
        bytes32 prevStateRoot,              // pre-state input to verifier
        bytes32 newStateRoot,               // verifier output
        bytes32 expectedQueue2,             // verifier input
        bytes32 updatedQueue2,              // verifier output path 1
        bytes32 newWithdrawalsOnly          // verifier output path 2
    );

    event WithdrawalProcessed(
        uint64 indexed zoneId,
        address indexed to,
        uint128 amount,
        bool callbackSuccess
    );

    event BounceBack(
        uint64 indexed zoneId,
        bytes32 indexed newCurrentDepositsHash,
        address indexed fallbackRecipient,
        uint128 amount
    );

    error NotSequencer();
    error InvalidProof();
    error NoWithdrawals();
    error InvalidWithdrawal();
    error UnexpectedQueue2State();
    error CallbackRejected();

    function zoneId() external view returns (uint64);
    function token() external view returns (address);
    function sequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
    function verifier() external view returns (address);
    function batchIndex() external view returns (uint64);
    function stateRoot() external view returns (bytes32);
    function processedDepositsHash() external view returns (bytes32);
    function pendingDepositsHash() external view returns (bytes32);
    function currentDepositsHash() external view returns (bytes32);
    function withdrawalQueue1() external view returns (bytes32);
    function withdrawalQueue2() external view returns (bytes32);

    function setSequencerPubkey(bytes32 pubkey) external;
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositsHash);
    function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external;
    function submitBatch(
        BatchCommitment calldata commitment,
        bytes32 expectedQueue2,
        bytes32 updatedQueue2,
        bytes32 newWithdrawalsOnly,
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
