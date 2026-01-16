// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZoneTypes} from "./IZoneTypes.sol";

/// @title IZoneOutbox
/// @notice Zone predeploy for withdrawal management
interface IZoneOutbox is IZoneTypes {
    /// @notice Emitted when a withdrawal is requested
    event WithdrawalRequested(bytes32 indexed withdrawalId, uint64 indexed withdrawalIndex);

    error InsufficientBalance();
    error InvalidAmount();

    /// @notice Get the next withdrawal index
    function nextWithdrawalIndex() external view returns (uint64);

    /// @notice Get a withdrawal by index
    function withdrawalByIndex(uint64 index) external view returns (Withdrawal memory);

    /// @notice Request a withdrawal from the zone to Tempo
    /// @param to The Tempo recipient address
    /// @param amount The amount to withdraw
    /// @param gasLimit Max gas for IExitReceiver callback (0 = no callback)
    /// @param fallbackRecipient Zone address for bounce-back if callback fails
    /// @param data Calldata for IExitReceiver (if gasLimit > 0)
    /// @return withdrawalId The withdrawal ID
    function requestWithdrawal(
        address to,
        uint128 amount,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes calldata data
    ) external returns (bytes32 withdrawalId);
}
