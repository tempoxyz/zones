// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZoneTypes} from "./IZoneTypes.sol";

/// @title IZoneOutbox
/// @notice Zone predeploy for exit intent management
interface IZoneOutbox is IZoneTypes {
    /// @notice Emitted when an exit is requested
    event ExitRequested(bytes32 indexed exitId, uint64 indexed exitIndex, ExitKind kind);

    error InsufficientBalance();
    error InvalidAmount();

    /// @notice Get the next exit index
    function nextExitIndex() external view returns (uint64);

    /// @notice Get an exit intent by index
    function exitByIndex(uint64 exitIndex) external view returns (ExitIntent memory);

    /// @notice Request a transfer exit
    /// @param recipient The Tempo recipient address
    /// @param amount The amount to transfer
    /// @param memo Optional memo
    /// @return exitId The exit intent ID
    function requestTransferExit(address recipient, uint128 amount, bytes32 memo) external returns (bytes32 exitId);

    /// @notice Request a swap exit
    /// @param tokenOut The output token address on Tempo
    /// @param amountIn The input amount (gas token)
    /// @param minAmountOut Minimum output amount (slippage protection)
    /// @param recipient The recipient on Tempo
    /// @param memo Optional memo
    /// @return exitId The exit intent ID
    function requestSwapExit(
        address tokenOut,
        uint128 amountIn,
        uint128 minAmountOut,
        address recipient,
        bytes32 memo
    ) external returns (bytes32 exitId);

    /// @notice Request a swap and deposit exit
    /// @param tokenOut The output token (must be destination zone's gas token)
    /// @param amountIn The input amount (gas token)
    /// @param minAmountOut Minimum output amount
    /// @param destinationZoneId The destination zone ID
    /// @param destinationRecipient The recipient on the destination zone
    /// @param memo Optional memo
    /// @return exitId The exit intent ID
    function requestSwapAndDepositExit(
        address tokenOut,
        uint128 amountIn,
        uint128 minAmountOut,
        uint64 destinationZoneId,
        address destinationRecipient,
        bytes32 memo
    ) external returns (bytes32 exitId);
}
