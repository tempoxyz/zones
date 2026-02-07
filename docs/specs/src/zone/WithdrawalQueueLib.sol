// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Withdrawal, WithdrawalQueueTransition } from "./IZone.sol";

/// @dev Sentinel value for empty slots. Using 0xff...ff instead of 0x00 to avoid
///      clearing storage (which would refund gas and create incentive issues).
bytes32 constant EMPTY_SENTINEL = bytes32(type(uint256).max);

/// @title WithdrawalQueue
/// @notice Unbounded buffer for zone→Tempo withdrawals
/// @dev Each batch gets its own slot. Head points to the oldest unprocessed batch,
///      tail points to where the next batch will write. Slots contain hash chains
///      of withdrawals for that batch.
///
// **REVIEWTODO: I don't like this specal gas accounting for the withdrawal queue, it creates some special casing that will be a nightmare later.
//               Let's try to find a different solution for this.
///      Gas note: This is implemented as a precompile on Tempo. Storage gas should
///      only be charged when (tail - head) > maxSize, i.e., when the queue length
///      exceeds its previous maximum. This allows the queue to shrink and regrow
///      without repeated storage charges.
struct WithdrawalQueue {
    uint256 head;     // slot index of oldest unprocessed batch
    uint256 tail;     // slot index where next batch will write
    uint256 maxSize;  // maximum queue length ever reached (for gas accounting)
    mapping(uint256 => bytes32) slots;  // hash chains per batch (EMPTY_SENTINEL = empty)
}

/// @title WithdrawalQueueLib
/// @notice Library for managing the withdrawal queue unbounded buffer
/// @dev Withdrawals are inserted by proofs (one slot per batch) and dequeued
///      on-chain by the sequencer. The sequencer processes withdrawals from
///      the head slot, advancing head when the slot is exhausted.
///
///      Invariants:
///      - Slots between head (inclusive) and tail (exclusive) contain withdrawal hash chains
///      - If head == tail, the queue is empty
///      - Slots at head contain EMPTY_SENTINEL only after being fully processed
library WithdrawalQueueLib {
    error NoWithdrawalsInQueue();
    error InvalidWithdrawalHash();

    /// @notice Add a batch's withdrawals to the queue
    /// @dev Called during submitBatch. The batch's withdrawal hash chain goes into
    ///      the slot at tail, then tail advances.
    /// @param queue The withdrawal queue
    /// @param transition The withdrawal queue transition containing the hash chain
    function enqueue(
        WithdrawalQueue storage queue,
        WithdrawalQueueTransition memory transition
    ) internal {
        // If no withdrawals in this batch, nothing to do
        if (transition.withdrawalQueueHash == bytes32(0)) {
            return;
        }

        uint256 tail = queue.tail;

        // Write the withdrawal hash chain to this slot
        queue.slots[tail] = transition.withdrawalQueueHash;

        // Advance tail
        queue.tail = tail + 1;

        // Update maxSize if current queue length exceeds previous maximum
        // Note: Gas charging for new storage should only happen when this increases
        uint256 currentSize = queue.tail - queue.head;
        if (currentSize > queue.maxSize) {
            queue.maxSize = currentSize;
        }
    }

    /// @notice Pop the next withdrawal from the queue
    /// @dev Verifies the withdrawal is at the head of the current slot and advances.
    ///      When a slot is exhausted (remainingQueue would be empty), we set it to
    ///      EMPTY_SENTINEL and advance head to the next slot.
    /// @param queue The withdrawal queue
    /// @param withdrawal The withdrawal to pop (must be at head of current slot)
    /// @param remainingQueue The hash of the remaining queue after this withdrawal
    function dequeue(
        WithdrawalQueue storage queue,
        Withdrawal calldata withdrawal,
        bytes32 remainingQueue
    ) internal {
        uint256 head = queue.head;

        // Check if queue is empty
        if (head == queue.tail) {
            revert NoWithdrawalsInQueue();
        }

        // Get the current slot's hash chain
        bytes32 currentSlot = queue.slots[head];

        // Verify this is the head of the current slot
        // Queue structure: oldest withdrawal at outermost layer for O(1) removal
        // The remainingQueue for the last item should be EMPTY_SENTINEL, not 0
        // **REVIEWTODO: Why wouldn't we just make remainingQueue = EMPTY_SENTINEL when it's empty?**
        bytes32 expectedRemainingQueue = remainingQueue == bytes32(0) ? EMPTY_SENTINEL : remainingQueue;
        if (keccak256(abi.encode(withdrawal, expectedRemainingQueue)) != currentSlot) {
            revert InvalidWithdrawalHash();
        }

        // Update the slot
        if (remainingQueue == bytes32(0)) {
            // Slot exhausted, clear and advance head
            queue.slots[head] = EMPTY_SENTINEL;
            queue.head = head + 1;
        } else {
            // More withdrawals in this slot
            queue.slots[head] = remainingQueue;
        }
    }

    /// @notice Check if the queue has any pending withdrawals
    /// @param queue The withdrawal queue
    /// @return True if there are withdrawals to process
    function hasWithdrawals(WithdrawalQueue storage queue) internal view returns (bool) {
        return queue.head != queue.tail;
    }

    /// @notice Get current queue length
    /// @param queue The withdrawal queue
    /// @return The number of batch slots with pending withdrawals
    function length(WithdrawalQueue storage queue) internal view returns (uint256) {
        return queue.tail - queue.head;
    }
}
