// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/**
 * @title DirectSwapErrorsAndEvents
 * @author Bridge
 * @notice Custom errors and events for the DirectSwap protocol
 * @dev Shared across contracts for consistency in error handling and event emissions
 */

/// @notice Thrown when the fee is invalid, e.g., > 100%
error InvalidFeeBps();
/// @notice Thrown when a zero address is provided
error ZeroAddress();
/// @notice Thrown when the token addresses arent correct
error InvalidTokenAddress();
/// @notice Thrown when the caller is not allowed
error UnauthorizedCaller();
/// @notice Thrown when the amount is zero
error InvalidAmount();
/// @notice Thrown when the authorization policy ID is invalid
error InvalidAuthPolicyId();
/// @notice Thrown when the swap amounts are invalid
error InvalidSwapAmounts(uint256 amountIn, uint256 feeAmount, uint256 amountOut, uint256 diff);
/// @notice Thrown when the token handler is invalid
error InvalidTokenHandler();
/// @notice Thrown when the token decimals are invalid
error InvalidTokenDecimals();
/// @notice Thrown when the token is not supported
error UnsupportedToken();
/// @notice Thrown when the stablecoin is not registered
error StablecoinNotRegistered();
/// @notice Thrown when the stablecoin is already registered
error StablecoinAlreadyRegistered();

/*//////////////////////////////////////////////////////////////
                            EVENTS
//////////////////////////////////////////////////////////////*/

/**
 * @notice Emitted when a swap is executed via DirectSwapV2
 * @param swapper Address that initiated the swap
 * @param tokenIn Address of the input token
 * @param tokenOut Address of the output token
 * @param amountIn Amount of input tokens transferred
 * @param amountOut Amount of output tokens received
 * @param feeAmount Amount of fee tokens transferred
 * @param recipient Address that received the output tokens
 */
event Swap(
    address indexed swapper,
    address indexed tokenIn,
    address indexed tokenOut,
    uint256 amountIn,
    uint256 amountOut,
    uint256 feeAmount,
    address recipient
);
