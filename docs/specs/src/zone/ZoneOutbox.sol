// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZoneGasToken } from "./ZoneInbox.sol";
import { Withdrawal } from "./IZone.sol";
import { EMPTY_SENTINEL } from "./WithdrawalQueueLib.sol";

/// @title ZoneOutbox
/// @notice Zone-side predeploy for requesting withdrawals back to Tempo
/// @dev Burns gas tokens and stores pending withdrawals. Sequencer calls batch() to
///      construct withdrawal queue hash on-chain.
contract ZoneOutbox {
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The gas token (TIP-20 at same address as L1)
    IZoneGasToken public immutable gasToken;

    /// @notice The sequencer address (only sequencer can call batch)
    address public immutable sequencer;

    /// @notice Next withdrawal index (monotonically increasing)
    uint64 public nextWithdrawalIndex;

    /// @notice Pending withdrawals waiting to be batched
    Withdrawal[] internal _pendingWithdrawals;

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

    /// @notice Emitted when sequencer batches pending withdrawals
    event BatchWithdrawals(bytes32 indexed withdrawalQueueHash, uint256 count);

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error InvalidFallbackRecipient();
    error TransferFailed();
    error OnlySequencer();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _gasToken, address _sequencer) {
        gasToken = IZoneGasToken(_gasToken);
        sequencer = _sequencer;
    }

    /*//////////////////////////////////////////////////////////////
                          WITHDRAWAL REQUESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Request a withdrawal from the zone back to Tempo
    /// @dev Caller must have approved the outbox to spend `amount` of gas tokens.
    ///      The outbox burns the tokens and stores the withdrawal. The sequencer
    ///      calls batch() to construct the withdrawal queue hash.
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
        // If callback requested, must have valid fallback recipient
        if (gasLimit > 0 && fallbackRecipient == address(0)) {
            revert InvalidFallbackRecipient();
        }

        // Transfer tokens from sender to this contract, then burn
        // (Using transferFrom so user must approve first)
        if (!gasToken.transferFrom(msg.sender, address(this), amount)) {
            revert TransferFailed();
        }

        // Burn the tokens (they'll be released on L1 when withdrawal is processed)
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

    /// @notice Build withdrawal hash chain and clear pending withdrawals
    /// @dev Only callable by sequencer. Builds hash chain with oldest withdrawal
    ///      outermost (matching L1 expectations for O(1) dequeue).
    /// @param count Max number of withdrawals to process (avoids unbounded loops)
    /// @return withdrawalQueueHash The hash chain (0 if no withdrawals)
    function batch(uint256 count) external returns (bytes32 withdrawalQueueHash) {
        if (msg.sender != sequencer) revert OnlySequencer();

        uint256 pending = _pendingWithdrawals.length;
        if (pending == 0 || count == 0) {
            return bytes32(0);
        }

        // Clamp to actual pending count
        if (count > pending) {
            count = pending;
        }

        // Build hash chain in reverse order (newest to oldest)
        // So oldest ends up outermost, matching L1 expectations
        withdrawalQueueHash = EMPTY_SENTINEL;

        for (uint256 i = 0; i < count; ) {
            // Always read from current end (array shrinks each iteration)
            Withdrawal memory w = _pendingWithdrawals[_pendingWithdrawals.length - 1];
            withdrawalQueueHash = keccak256(abi.encode(w, withdrawalQueueHash));
            _pendingWithdrawals.pop();
            unchecked { i++; }
        }

        emit BatchWithdrawals(withdrawalQueueHash, count);
    }

    /// @notice Number of pending withdrawals
    function pendingWithdrawalsCount() external view returns (uint256) {
        return _pendingWithdrawals.length;
    }
}
