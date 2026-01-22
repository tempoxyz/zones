// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZoneGasToken } from "./ZoneInbox.sol";
import { IZoneOutbox, Withdrawal, LastBatch } from "./IZone.sol";
import { EMPTY_SENTINEL } from "./WithdrawalQueueLib.sol";

/// @title ZoneOutbox
/// @notice Zone-side predeploy for requesting withdrawals back to Tempo
/// @dev Burns gas tokens and stores pending withdrawals. Sequencer calls finalizeWithdrawalBatch()
///      at the end of a block to construct withdrawal queue hash on-chain.
contract ZoneOutbox is IZoneOutbox {
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The gas token (TIP-20 at same address as Tempo)
    IZoneGasToken public immutable gasToken;

    /// @notice The sequencer address (only sequencer can call finalizeWithdrawalBatch)
    address public immutable sequencer;

    /// @notice Next withdrawal index (monotonically increasing)
    uint64 public nextWithdrawalIndex;

    /// @notice Current withdrawal batch index (monotonically increasing)
    uint64 public withdrawalBatchIndex;

    /// @notice Last finalized batch parameters (for proof access via state root)
    /// @dev Written on each finalizeWithdrawalBatch() call so proofs can read from state
    ///      instead of parsing event logs
    LastBatch internal _lastBatch;

    /// @notice Pending withdrawals waiting to be batched
    Withdrawal[] internal _pendingWithdrawals;
    uint256 internal _pendingWithdrawalsHead;

    /*//////////////////////////////////////////////////////////////
                                EVENTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Emitted when a user requests a withdrawal
    /// @dev Kept for observability, even though hash is now built on-chain
    event WithdrawalRequested(
        uint64 indexed withdrawalIndex,
        address indexed sender,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes data
    );

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error InvalidFallbackRecipient();
    error TransferFailed();
    error OnlySequencer();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(
        address _gasToken,
        address _sequencer
    ) {
        gasToken = IZoneGasToken(_gasToken);
        sequencer = _sequencer;
    }

    /*//////////////////////////////////////////////////////////////
                          WITHDRAWAL REQUESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Request a withdrawal from the zone back to Tempo
    /// @dev Caller must have approved the outbox to spend `amount` of gas tokens.
    ///      The outbox burns the tokens and stores the withdrawal. The sequencer
    ///      calls finalizeWithdrawalBatch() to construct the withdrawal queue hash.
    /// @param to The Tempo recipient address
    /// @param amount Amount to withdraw
    /// @param memo User-provided context (e.g., payment reference)
    /// @param gasLimit Gas limit for IWithdrawalReceiver callback (0 = no callback)
    /// @param fallbackRecipient Zone address for bounce-back if callback fails
    /// @param data Calldata for IWithdrawalReceiver callback
    function requestWithdrawal(
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes calldata data
    ) external {
        // Always require a valid fallback recipient
        if (fallbackRecipient == address(0)) {
            revert InvalidFallbackRecipient();
        }

        // Transfer tokens from sender to this contract, then burn
        // (Using transferFrom so user must approve first)
        if (!gasToken.transferFrom(msg.sender, address(this), amount)) {
            revert TransferFailed();
        }

        // Burn the tokens (they'll be released on Tempo when withdrawal is processed)
        gasToken.burn(address(this), amount);

        // Store withdrawal in pending array
        _pendingWithdrawals.push(Withdrawal({
            sender: msg.sender,
            to: to,
            amount: amount,
            memo: memo,
            gasLimit: gasLimit,
            fallbackRecipient: fallbackRecipient,
            callbackData: data
        }));

        // Emit event for observability
        uint64 index = nextWithdrawalIndex++;

        emit WithdrawalRequested(
            index,
            msg.sender,
            to,
            amount,
            memo,
            gasLimit,
            fallbackRecipient,
            data
        );
    }

    /*//////////////////////////////////////////////////////////////
                              BATCH OPERATIONS
    //////////////////////////////////////////////////////////////*/

    /// @notice Finalize the batch at end of block - build withdrawal hash and emit proof inputs
    /// @dev Only callable by sequencer as a system transaction at the end of a block.
    ///      The proof enforces that this is the last call in the block and that a batch
    ///      ends with exactly one finalizeWithdrawalBatch call (use count = 0 if no withdrawals).
    ///      Protocol and proof enforce this runs at the end of the final block in the batch.
    ///      Emits BatchFinalized for observability (proof reads from state).
    /// @param count Max number of withdrawals to process (avoids unbounded loops)
    /// @return withdrawalQueueHash The hash chain (0 if no withdrawals)
    function finalizeWithdrawalBatch(uint256 count) external returns (bytes32 withdrawalQueueHash) {
        if (msg.sender != sequencer) revert OnlySequencer();

        uint256 pending = _pendingWithdrawals.length - _pendingWithdrawalsHead;

        // Clamp to actual pending count
        if (count > pending) {
            count = pending;
        }

        // Build hash chain in reverse order (newest to oldest)
        // So oldest ends up outermost, matching Tempo expectations.
        // Process the oldest withdrawals first (FIFO).
        if (count > 0) {
            withdrawalQueueHash = EMPTY_SENTINEL;

            uint256 start = _pendingWithdrawalsHead;
            uint256 end = start + count;

            for (uint256 i = end; i > start; ) {
                uint256 index = i - 1;
                Withdrawal memory w = _pendingWithdrawals[index];
                withdrawalQueueHash = keccak256(abi.encode(w, withdrawalQueueHash));
                delete _pendingWithdrawals[index];
                unchecked { i--; }
            }

            _pendingWithdrawalsHead = end;

            if (_pendingWithdrawalsHead == _pendingWithdrawals.length) {
                delete _pendingWithdrawals;
                _pendingWithdrawalsHead = 0;
            }
        }

        // Increment withdrawal batch index (matches Tempo portal's next expected withdrawal batch index)
        withdrawalBatchIndex += 1;
        uint64 currentWithdrawalBatchIndex = withdrawalBatchIndex;

        // Write withdrawal batch parameters to state (for proof access via state root)
        _lastBatch = LastBatch({
            withdrawalQueueHash: withdrawalQueueHash,
            withdrawalBatchIndex: currentWithdrawalBatchIndex
        });

        // Emit event for observability (proof reads from state, not events)
        emit BatchFinalized(
            withdrawalQueueHash,
            currentWithdrawalBatchIndex
        );
    }

    /// @notice Number of pending withdrawals
    function pendingWithdrawalsCount() external view returns (uint256) {
        if (_pendingWithdrawalsHead >= _pendingWithdrawals.length) {
            return 0;
        }
        return _pendingWithdrawals.length - _pendingWithdrawalsHead;
    }

    /// @notice Last finalized batch parameters (for proof access via state root)
    function lastBatch() external view returns (LastBatch memory) {
        return _lastBatch;
    }
}
