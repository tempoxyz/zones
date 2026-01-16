// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title IExitReceiver
/// @notice Interface for contracts that receive withdrawal callbacks
interface IExitReceiver {
    /// @notice Called by the portal when a withdrawal targets this contract
    /// @param sender The address that initiated the withdrawal on the zone
    /// @param amount The amount of gas tokens transferred
    /// @param data User-provided data from the withdrawal
    /// @return selector Must return IExitReceiver.onExitReceived.selector to accept
    function onExitReceived(
        address sender,
        uint128 amount,
        bytes calldata data
    ) external returns (bytes4);
}
