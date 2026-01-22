// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Deposit } from "./IZone.sol";
import { TempoState } from "./TempoState.sol";

/// @title IZoneGasToken
/// @notice Interface for the zone's gas token (TIP-20 with mint/burn for system)
interface IZoneGasToken {
    function mint(address to, uint256 amount) external;
    function burn(address from, uint256 amount) external;
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

/// @title ZoneInbox
/// @notice Zone-side system contract for advancing Tempo state and processing deposits
/// @dev Called by sequencer as a system transaction. Combines Tempo header advancement
///      with deposit queue processing in a single atomic operation.
contract ZoneInbox {
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The Tempo portal address (for reading deposit queue hash)
    address public immutable tempoPortal;

    /// @notice The TempoState predeploy address
    TempoState public immutable tempoState;

    /// @notice The gas token (TIP-20 at same address as Tempo)
    IZoneGasToken public immutable gasToken;

    /// @notice Current sequencer address
    address public sequencer;

    /// @notice Pending sequencer for two-step transfer
    address public pendingSequencer;

    /// @notice Last processed deposit queue hash (validated against Tempo state)
    bytes32 public processedDepositQueueHash;

    /// @notice Storage slot for currentDepositQueueHash in ZonePortal
    /// @dev ZonePortal storage layout (non-immutable variables only):
    ///      slot 0: sequencer (address)
    ///      slot 1: pendingSequencer (address)
    ///      slot 2: sequencerPubkey (bytes32)
    ///      slot 3: withdrawalBatchIndex (uint64)
    ///      slot 4: blockHash (bytes32)
    ///      slot 5: currentDepositQueueHash (bytes32) ← this one
    ///      slot 6: lastSyncedTempoBlockNumber (uint64)
    bytes32 internal constant CURRENT_DEPOSIT_QUEUE_HASH_SLOT = bytes32(uint256(5));

    /*//////////////////////////////////////////////////////////////
                                EVENTS
    //////////////////////////////////////////////////////////////*/

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

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error OnlySequencer();
    error NotPendingSequencer();
    error InvalidDepositQueueHash();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(
        address _tempoPortal,
        address _tempoState,
        address _gasToken,
        address _sequencer
    ) {
        tempoPortal = _tempoPortal;
        tempoState = TempoState(_tempoState);
        gasToken = IZoneGasToken(_gasToken);
        sequencer = _sequencer;
    }

    /*//////////////////////////////////////////////////////////////
                         SEQUENCER MANAGEMENT
    //////////////////////////////////////////////////////////////*/

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    function transferSequencer(address newSequencer) external {
        if (msg.sender != sequencer) revert OnlySequencer();
        pendingSequencer = newSequencer;
        emit SequencerTransferStarted(sequencer, newSequencer);
    }

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external {
        if (msg.sender != pendingSequencer) revert NotPendingSequencer();
        address previousSequencer = sequencer;
        sequencer = pendingSequencer;
        pendingSequencer = address(0);
        emit SequencerTransferred(previousSequencer, sequencer);
    }

    /*//////////////////////////////////////////////////////////////
                         SYSTEM TRANSACTION
    //////////////////////////////////////////////////////////////*/

    /// @notice Advance Tempo state and process deposits in a single system transaction
    /// @dev This is the main entry point for the sequencer's system transaction.
    ///      1. Advances the zone's view of Tempo by processing the header
    ///      2. Processes deposits from the deposit queue
    ///      3. Validates the resulting hash against Tempo's currentDepositQueueHash
    ///      Protocol and proof enforce this runs at the start of each block.
    /// @param header RLP-encoded Tempo block header
    /// @param deposits Array of deposits to process (oldest first, must be contiguous from processedDepositQueueHash)
    function advanceTempo(
        bytes calldata header,
        Deposit[] calldata deposits
    ) external {
        if (msg.sender != sequencer) revert OnlySequencer();

        // Step 1: Advance Tempo state (validates chain continuity internally)
        tempoState.finalizeTempo(header);

        // Step 2: Process deposits and build hash chain
        bytes32 currentHash = processedDepositQueueHash;

        for (uint256 i = 0; i < deposits.length; i++) {
            Deposit calldata d = deposits[i];

            // Advance the hash chain (matches Tempo's deposit queue structure)
            currentHash = keccak256(abi.encode(d, currentHash));

            // Mint gas tokens to the recipient
            gasToken.mint(d.to, d.amount);

            emit DepositProcessed(currentHash, d.sender, d.to, d.amount, d.memo);
        }

        // Step 3: Validate against Tempo state
        // Read currentDepositQueueHash from the portal's storage using the new Tempo state
        bytes32 tempoCurrentHash = tempoState.readTempoStorageSlot(
            tempoPortal,
            CURRENT_DEPOSIT_QUEUE_HASH_SLOT
        );

        // Our processed hash must match Tempo's current hash for now.
        // TODO: Implement recursive ancestor check in proof or on-chain as a fallback.
        if (currentHash != tempoCurrentHash) {
            revert InvalidDepositQueueHash();
        }

        // Step 4: Update state
        processedDepositQueueHash = currentHash;

        emit TempoAdvanced(
            tempoState.tempoBlockHash(),
            tempoState.tempoBlockNumber(),
            deposits.length,
            currentHash
        );
    }
}
