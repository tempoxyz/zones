// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import { IERC20Like } from "../interfaces/external/IERC20Like.sol";
import { ISwapAdapter } from "../interfaces/periphery/ISwapAdapter.sol";

interface IBridgeDirectSwapV2 {
    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn) external;
}

/// @notice Adapts Bridge DirectSwapV2 to the generic Earn swap override interface.
contract BridgeDirectSwapAdapter is ISwapAdapter {
    address public immutable directSwap;
    address public immutable tokenA;
    address public immutable tokenB;

    error InsufficientOutput();
    error InvalidToken();
    error SameToken();
    error TokenCallFailed();
    error TokenCallFalse();
    error ZeroAddress();
    error ZeroAmount();

    constructor(address directSwap_, address tokenA_, address tokenB_) {
        if (directSwap_ == address(0) || tokenA_ == address(0) || tokenB_ == address(0)) revert ZeroAddress();
        if (tokenA_ == tokenB_) revert SameToken();

        directSwap = directSwap_;
        tokenA = tokenA_;
        tokenB = tokenB_;
    }

    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn, address receiver, uint256 minAmountOut)
        external
        returns (uint256 amountOut)
    {
        if (amountIn == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();

        _validatePair(tokenIn, tokenOut);
        uint256 beforeBalance = IERC20Like(tokenOut).balanceOf(address(this));

        _safeTransferFrom(tokenIn, msg.sender, address(this), amountIn);
        _safeApprove(tokenIn, directSwap, 0);
        _safeApprove(tokenIn, directSwap, amountIn);
        IBridgeDirectSwapV2(directSwap).swapExactIn(tokenIn, tokenOut, amountIn);

        uint256 afterBalance = IERC20Like(tokenOut).balanceOf(address(this));
        if (afterBalance < beforeBalance) revert InsufficientOutput();
        amountOut = afterBalance - beforeBalance;
        if (amountOut == 0 || amountOut < minAmountOut) revert InsufficientOutput();

        _safeTransfer(tokenOut, receiver, amountOut);
    }

    function _validatePair(address tokenIn, address tokenOut) internal view {
        if (tokenIn == tokenA && tokenOut == tokenB) return;
        if (tokenIn == tokenB && tokenOut == tokenA) return;
        revert InvalidToken();
    }

    function _safeApprove(address token, address spender, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeWithSelector(IERC20Like.approve.selector, spender, value));
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
