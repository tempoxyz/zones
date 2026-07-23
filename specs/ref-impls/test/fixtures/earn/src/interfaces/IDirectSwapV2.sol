// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

/**
 * @title IDirectSwapV2
 * @author Bridge
 * @notice Interface for the DirectSwapV2 contract that facilitates atomic swaps
 * between backing asset tokens and stablecoins.
 */
interface IDirectSwapV2 {
    /**
     * @notice Swaps tokens with an exact output amount
     * @dev Output tokens are always sent to `msg.sender`.
     * @param tokenIn Address of the token being sold
     * @param tokenOut Address of the token being bought
     * @param amountOut Exact amount of output tokens to receive
     */
    function swapExactOut(address tokenIn, address tokenOut, uint256 amountOut) external;

    /**
     * @notice Swaps tokens with an exact input amount
     * @dev Output tokens are always sent to `msg.sender`.
     * @param tokenIn Address of the token being sold
     * @param tokenOut Address of the token being bought
     * @param amountIn Exact amount of input tokens to sell
     */
    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn) external;

    /**
     * @notice Returns the input amount required to receive `amountOut` output tokens via
     * `swapExactOut`.
     * @dev Mirrors the rounding used by `swapExactOut` (rounds up so the protocol does not
     * lose on rounding). Reverts on the same input validation as the swap (zero amount).
     * @param amountOut Exact amount of output tokens that would be received
     * @return amountIn Amount of input tokens that would be pulled from the caller
     */
    function quoteExactOut(uint256 amountOut) external view returns (uint256 amountIn);

    /**
     * @notice Returns the output amount that would be received for `amountIn` input tokens via
     * `swapExactIn`.
     * @dev Mirrors the rounding used by `swapExactIn` (rounds down to favor the protocol).
     * Reverts on the same input validation as the swap (zero amount).
     * @param amountIn Exact amount of input tokens that would be sold
     * @return amountOut Amount of output tokens that would be sent to the caller
     */
    function quoteExactIn(uint256 amountIn) external view returns (uint256 amountOut);
}
