// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZoneTypes} from "./IZoneTypes.sol";

/// @title IZonePortal
/// @notice Per-zone portal that escrows the gas token and processes withdrawals
interface IZonePortal is IZoneTypes {
    /// @notice Emitted when a deposit is enqueued
    event DepositEnqueued(
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

    /// @notice Emitted when a batch is submitted
    event BatchSubmitted(
        uint64 indexed zoneId,
        uint64 indexed batchIndex,
        bytes32 newProcessedDepositsHash,
        bytes32 newStateRoot
    );

    /// @notice Emitted when a withdrawal is processed
    event WithdrawalProcessed(
        uint64 indexed zoneId,
        address indexed to,
        uint128 amount
    );

    /// @notice Emitted when a withdrawal bounces back
    event WithdrawalBouncedBack(
        uint64 indexed zoneId,
        address indexed fallbackRecipient,
        uint128 amount
    );

    error OnlySequencer();
    error InvalidProof();
    error InvalidWithdrawal();
    error NoWithdrawals();
    error UnexpectedQueue2State();
    error WithdrawalRejected();

    function zoneId() external view returns (uint64);
    function gasToken() external view returns (address);
    function sequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
    function verifier() external view returns (address);
    function batchIndex() external view returns (uint64);
    function stateRoot() external view returns (bytes32);
    function currentDepositsHash() external view returns (bytes32);
    function checkpointedDepositsHash() external view returns (bytes32);
    function withdrawalQueue1() external view returns (bytes32);
    function withdrawalQueue2() external view returns (bytes32);

    /// @notice Set the sequencer's public key. Only callable by the sequencer.
    function setSequencerPubkey(bytes32 pubkey) external;

    /// @notice Deposit gas token into the zone
    /// @param to The recipient on the zone
    /// @param amount The amount to deposit
    /// @param memo Optional memo for the deposit
    /// @return newCurrentDepositsHash The new deposits hash chain head
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositsHash);

    /// @notice Process the next withdrawal from queue1. Only callable by the sequencer.
    /// @param w The withdrawal to process (must be at the head of queue1)
    /// @param remainingQueue The hash of the remaining queue after this withdrawal
    function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external;

    /// @notice Submit a batch and verify the proof. Only callable by the sequencer.
    /// @param commitment The batch commitment (new state root and processed deposits hash)
    /// @param expectedQueue2 The queue2 value the proof assumed during generation
    /// @param updatedQueue2 New queue2 if expectedQueue2 matches current queue2
    /// @param newWithdrawalsOnly New queue2 if current queue2 is empty (swap occurred)
    /// @param proof The validity proof or TEE attestation
    function submitBatch(
        BatchCommitment calldata commitment,
        bytes32 expectedQueue2,
        bytes32 updatedQueue2,
        bytes32 newWithdrawalsOnly,
        bytes calldata proof
    ) external;
}
