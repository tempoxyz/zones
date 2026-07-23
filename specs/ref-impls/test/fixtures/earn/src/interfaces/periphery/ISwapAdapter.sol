// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

interface ISwapAdapter {
    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn, address receiver, uint256 minAmountOut)
        external
        returns (uint256 amountOut);
}
