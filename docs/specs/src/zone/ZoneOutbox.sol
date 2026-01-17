// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZoneGasToken } from "./ZoneInbox.sol";

/// @title ZoneOutbox
/// @notice Zone-side predeploy for requesting withdrawals back to Tempo
/// @dev Burns gas tokens and emits events. Sequencer builds withdrawal queue from events.
contract ZoneOutbox {
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The gas token (TIP-20 at same address as L1)
    IZoneGasToken public immutable gasToken;

    /// @notice Next withdrawal index (monotonically increasing)
    uint64 public nextWithdrawalIndex;

    /*//////////////////////////////////////////////////////////////
                                EVENTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Emitted when a user requests a withdrawal
    /// @dev Sequencer collects these events to build the withdrawal queue hash
    event WithdrawalRequested(
        uint64 indexed withdrawalIndex,
        address indexed sender,
        address to,
        uint128 amount,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes data
    );

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error InvalidFallbackRecipient();
    error TransferFailed();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _gasToken) {
        gasToken = IZoneGasToken(_gasToken);
    }

    /*//////////////////////////////////////////////////////////////
                          WITHDRAWAL REQUESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Request a withdrawal from the zone back to Tempo
    /// @dev Caller must have approved the outbox to spend `amount` of gas tokens.
    ///      The outbox burns the tokens and emits an event. The sequencer collects
    ///      events to build the withdrawal queue for batch submission.
    /// @param to The Tempo recipient address
    /// @param amount Amount to withdraw
    /// @param gasLimit Gas limit for IExitReceiver callback (0 = no callback)
    /// @param fallbackRecipient Zone address for bounce-back if callback fails
    /// @param data Calldata for IExitReceiver callback
    function requestWithdrawal(
        address to,
        uint128 amount,
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

        // Emit event for sequencer to collect
        uint64 index = nextWithdrawalIndex++;

        emit WithdrawalRequested(
            index,
            msg.sender,
            to,
            amount,
            gasLimit,
            fallbackRecipient,
            data
        );
    }
}
