// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import {IERC20Like} from "../src/interfaces/IERC20Like.sol";

/// @notice Benchmark-only funded 1:1 swap fixture.
/// @dev This implements the three-argument DirectSwap interface consumed by
///      BridgeStableSwapAdapter. It is not a production swap implementation.
contract SimpleDirectSwapFixture {
    address public immutable tokenA;
    address public immutable tokenB;

    error InvalidToken();
    error SameToken();
    error TokenCallFailed();
    error TokenCallFalse();
    error ZeroAddress();
    error ZeroAmount();

    constructor(address tokenA_, address tokenB_) {
        if (tokenA_ == address(0) || tokenB_ == address(0)) revert ZeroAddress();
        if (tokenA_ == tokenB_) revert SameToken();
        tokenA = tokenA_;
        tokenB = tokenB_;
    }

    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn) external {
        if (amountIn == 0) revert ZeroAmount();
        _validatePair(tokenIn, tokenOut);
        _safeTransferFrom(tokenIn, msg.sender, address(this), amountIn);
        _safeTransfer(tokenOut, msg.sender, amountIn);
    }

    function swapExactOut(address tokenIn, address tokenOut, uint256 amountOut) external {
        if (amountOut == 0) revert ZeroAmount();
        _validatePair(tokenIn, tokenOut);
        _safeTransferFrom(tokenIn, msg.sender, address(this), amountOut);
        _safeTransfer(tokenOut, msg.sender, amountOut);
    }

    function quoteExactIn(uint256 amountIn) external pure returns (uint256 amountOut) {
        if (amountIn == 0) revert ZeroAmount();
        return amountIn;
    }

    function quoteExactOut(uint256 amountOut) external pure returns (uint256 amountIn) {
        if (amountOut == 0) revert ZeroAmount();
        return amountOut;
    }

    function _validatePair(address tokenIn, address tokenOut) internal view {
        if (tokenIn == tokenA && tokenOut == tokenB) return;
        if (tokenIn == tokenB && tokenOut == tokenA) return;
        revert InvalidToken();
    }

    function _safeTransfer(address token, address to, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeWithSelector(IERC20Like.transfer.selector, to, value));
    }

    function _safeTransferFrom(address token, address from, address to, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeWithSelector(IERC20Like.transferFrom.selector, from, to, value));
    }

    function _callOptionalReturn(address token, bytes memory data) internal {
        (bool ok, bytes memory returnData) = token.call(data);
        if (!ok) revert TokenCallFailed();
        if (returnData.length != 0 && !abi.decode(returnData, (bool))) revert TokenCallFalse();
    }
}
