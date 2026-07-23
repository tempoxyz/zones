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
     * @notice Swaps tokens with an exact input amount
     * @dev Output tokens are always sent to `msg.sender`.
     * @param tokenIn Address of the token being sold
     * @param tokenOut Address of the token being bought
     * @param amountIn Exact amount of input tokens to sell
     */
    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn) external;
}
