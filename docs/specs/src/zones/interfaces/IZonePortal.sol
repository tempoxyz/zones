// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZoneTypes} from "./IZoneTypes.sol";

/// @title IZonePortal
/// @notice Per-zone portal that escrows the gas token and finalizes exits
interface IZonePortal is IZoneTypes {
    /// @notice Emitted when a deposit is enqueued
    event DepositEnqueued(
        uint64 indexed zoneId,
        uint64 indexed depositIndex,
        address indexed sender,
        address to,
        uint256 amount,
        bytes32 memo
    );

    /// @notice Emitted when a batch is accepted
    event BatchAccepted(
        uint64 indexed zoneId,
        uint64 indexed batchIndex,
        bytes32 prevStateRoot,
        bytes32 newStateRoot,
        uint64 depositIndex,
        bytes32 exitRoot,
        bytes32 batchHash
    );

    /// @notice Emitted when an exit is finalized
    event ExitFinalized(bytes32 indexed exitId, uint64 indexed zoneId, ExitKind kind);

    error InvalidBatchIndex();
    error InvalidDepositIndex();
    error InvalidPrevStateRoot();
    error InvalidProof();
    error ExitAlreadyClaimed();
    error OnlySequencer();
    error SwapFailed();
    error DestinationZoneNotFound();

    /// @notice Get the zone ID
    function zoneId() external view returns (uint64);

    /// @notice Get the gas token address
    function gasToken() external view returns (address);

    /// @notice Get the sequencer address
    function sequencer() external view returns (address);

    /// @notice Get the verifier address
    function verifier() external view returns (address);

    /// @notice Get the next deposit index
    function nextDepositIndex() external view returns (uint64);

    /// @notice Get a deposit by index
    function deposits(uint64 index) external view returns (Deposit memory);

    /// @notice Get the current state root
    function stateRoot() external view returns (bytes32);

    /// @notice Get the current batch index
    function batchIndex() external view returns (uint64);

    /// @notice Get the current exit root
    function exitRoot() external view returns (bytes32);

    /// @notice Deposit tokens into the zone
    /// @param to The recipient on the zone
    /// @param amount The amount to deposit
    /// @param memo Optional memo for the deposit
    /// @return depositIndex The index of the deposit
    function deposit(address to, uint256 amount, bytes32 memo) external returns (uint64 depositIndex);

    /// @notice Submit a batch commitment with proof
    /// @param commitment The batch commitment
    /// @param proof The validity proof
    function submitBatch(BatchCommitment calldata commitment, bytes calldata proof) external;

    /// @notice Check if an exit has been claimed
    function exitClaimed(bytes32 exitId) external view returns (bool);

    /// @notice Finalize a transfer exit
    function finalizeTransferExit(ExitIntent calldata intent, ExitProof calldata proof) external;

    /// @notice Finalize a swap exit
    function finalizeSwapExit(ExitIntent calldata intent, ExitProof calldata proof) external;

    /// @notice Finalize a swap and deposit exit
    function finalizeSwapAndDepositExit(ExitIntent calldata intent, ExitProof calldata proof) external;
}
