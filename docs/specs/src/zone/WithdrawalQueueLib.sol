// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Withdrawal, WithdrawalQueueTransition } from "./IZone.sol";

/// @title WithdrawalQueue
/// @notice 2-slot queue for zone→L1 withdrawals with swap pattern
struct WithdrawalQueue {
    bytes32 active;   // active queue (being drained on-chain)
    bytes32 pending;  // pending queue (being filled by proofs)
}

/// @title WithdrawalQueueLib
/// @notice Library for managing the withdrawal queue hash chain
/// @dev Withdrawals are inserted by proofs and dequeued on-chain by the sequencer.
///      The 2-queue design handles the race where the sequencer drains active while a proof is in-flight:
///      - `active`: active queue being processed
///      - `pending`: pending queue receiving new withdrawals from proofs
///      When active empties, swap in pending.
///
///      Note on proof complexity: The off-chain proof that enqueues withdrawals is O(N) in the number
///      of pending withdrawals, since it must reconstruct the hash chain to append new items. While
///      this could theoretically become expensive if withdrawals accumulate faster than they are
///      processed, in practice the withdrawal queue proving costs are expected to be a small fraction
///      of the costs of proving zone blocks. If this becomes a concern, the design could be extended
///      with overflow queues to bound proof complexity, without changing the on-chain interface.
library WithdrawalQueueLib {
    error NoWithdrawalsInQueue();
    error InvalidWithdrawalHash();
    error UnexpectedPendingQueueHash();

    /// @notice Pop the next withdrawal from the queue (on-chain operation)
    /// @dev Verifies the withdrawal is at the head of active and advances the queue.
    ///      If active is empty, automatically swaps in pending first.
    /// @param queue The withdrawal queue
    /// @param withdrawal The withdrawal to pop (must be at head of queue)
    /// @param remainingQueue The hash of the remaining queue after this withdrawal
    function dequeue(
        WithdrawalQueue storage queue,
        Withdrawal calldata withdrawal,
        bytes32 remainingQueue
    ) internal {
        // Swap in pending if active is empty
        if (queue.active == bytes32(0)) {
            if (queue.pending == bytes32(0)) revert NoWithdrawalsInQueue();
            queue.active = queue.pending;
            queue.pending = bytes32(0);
        }

        // Verify this is the head of active
        // Queue structure: oldest withdrawal at outermost layer for O(1) removal
        if (keccak256(abi.encode(withdrawal, remainingQueue)) != queue.active) {
            revert InvalidWithdrawalHash();
        }

        // Advance active
        if (remainingQueue == bytes32(0)) {
            // Active exhausted, swap in pending
            queue.active = queue.pending;
            queue.pending = bytes32(0);
        } else {
            queue.active = remainingQueue;
        }
    }

    /// @notice Enqueue new withdrawals via proof (proof operation)
    /// @dev Called when a batch proof is submitted. The proof computed new withdrawals
    ///      and provides two outputs to handle the race condition:
    ///      - `nextPendingHashIfNoSwap`: pending queue with new withdrawals appended
    ///      - `nextPendingHashIfSwapped`: new withdrawals only (pending was swapped away)
    ///
    ///      The race condition:
    ///      1. Proof starts, sees pending = X
    ///      2. Sequencer drains active, triggers swap: active = X, pending = 0
    ///      3. Proof submits expecting pending = X, but it's now 0
    ///
    ///      Solution: proof provides both outputs, we use the appropriate one.
    /// @param queue The withdrawal queue
    /// @param transition The withdrawal queue transition containing prev/next state hashes
    function enqueueWithProof(
        WithdrawalQueue storage queue,
        WithdrawalQueueTransition memory transition
    ) internal {
        if (queue.pending == transition.prevPendingHash) {
            // No swap happened during proving, use the appended version
            queue.pending = transition.nextPendingHashIfNoSwap;
        } else if (queue.pending == bytes32(0)) {
            // Swap happened during proving, pending is now empty, use fresh
            queue.pending = transition.nextPendingHashIfSwapped;
        } else {
            // Unexpected state: pending is neither expected nor empty
            revert UnexpectedPendingQueueHash();
        }
    }

    /// @notice Get the current state for proof generation
    /// @param queue The withdrawal queue
    /// @return activeQueueHash Active queue being drained
    /// @return pendingQueueHash Pending queue being filled
    function getState(
        WithdrawalQueue storage queue
    ) internal view returns (bytes32 activeQueueHash, bytes32 pendingQueueHash) {
        return (queue.active, queue.pending);
    }

    /// @notice Check if there are any pending withdrawals
    /// @param queue The withdrawal queue
    /// @return True if either queue has withdrawals
    function hasWithdrawals(WithdrawalQueue storage queue) internal view returns (bool) {
        return queue.active != bytes32(0) || queue.pending != bytes32(0);
    }
}
