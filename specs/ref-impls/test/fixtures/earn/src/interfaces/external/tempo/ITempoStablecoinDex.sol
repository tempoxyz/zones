// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

interface ITempoStablecoinDex {
    function swapExactAmountIn(address tokenIn, address tokenOut, uint128 amountIn, uint128 minAmountOut)
        external
        returns (uint128 amountOut);
}
