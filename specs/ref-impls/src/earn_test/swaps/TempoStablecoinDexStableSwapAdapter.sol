// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC20Like } from "../interfaces/IERC20Like.sol";
import { IStableSwap } from "../interfaces/IStableSwap.sol";
import { ITempoStablecoinDex } from "../interfaces/ITempoStablecoinDex.sol";

contract TempoStablecoinDexStableSwapAdapter is IStableSwap {

    address public immutable stablecoinDex;

    error AmountOverflow();
    error InsufficientOutput();
    error SameToken();
    error TokenCallFailed();
    error TokenCallFalse();
    error ZeroAddress();
    error ZeroAmount();

    constructor(address stablecoinDex_) {
        if (stablecoinDex_ == address(0)) revert ZeroAddress();
        stablecoinDex = stablecoinDex_;
    }

    function swapExactIn(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        uint256 minAmountOut
    )
        external
        returns (uint256 amountOut)
    {
        if (tokenIn == address(0) || tokenOut == address(0) || receiver == address(0)) {
            revert ZeroAddress();
        }
        if (tokenIn == tokenOut) revert SameToken();
        if (amountIn == 0) revert ZeroAmount();
        uint128 amountIn128 = _toUint128(amountIn);
        uint128 minAmountOut128 = _toUint128(minAmountOut);

        uint256 beforeBalance = IERC20Like(tokenOut).balanceOf(address(this));

        _safeTransferFrom(tokenIn, msg.sender, address(this), amountIn);
        _safeApprove(tokenIn, stablecoinDex, 0);
        _safeApprove(tokenIn, stablecoinDex, amountIn);
        uint128 dexAmountOut = ITempoStablecoinDex(stablecoinDex)
            .swapExactAmountIn(tokenIn, tokenOut, amountIn128, minAmountOut128);
        if (dexAmountOut < minAmountOut128) revert InsufficientOutput();

        uint256 afterBalance = IERC20Like(tokenOut).balanceOf(address(this));
        if (afterBalance < beforeBalance) revert InsufficientOutput();
        amountOut = afterBalance - beforeBalance;
        if (amountOut == 0 || amountOut < minAmountOut) revert InsufficientOutput();

        _safeTransfer(tokenOut, receiver, amountOut);
    }

    function _toUint128(uint256 value) internal pure returns (uint128) {
        if (value > type(uint128).max) revert AmountOverflow();
        // forge-lint: disable-next-line(unsafe-typecast)
        return uint128(value);
    }

    function _safeApprove(address token, address spender, uint256 value) internal {
        _callOptionalReturn(
            token, abi.encodeWithSelector(IERC20Like.approve.selector, spender, value)
        );
    }

    function _safeTransfer(address token, address to, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeWithSelector(IERC20Like.transfer.selector, to, value));
    }

    function _safeTransferFrom(address token, address from, address to, uint256 value) internal {
        _callOptionalReturn(
            token, abi.encodeWithSelector(IERC20Like.transferFrom.selector, from, to, value)
        );
    }

    function _callOptionalReturn(address token, bytes memory data) internal {
        (bool ok, bytes memory returnData) = token.call(data);
        if (!ok) revert TokenCallFailed();
        if (returnData.length != 0 && !abi.decode(returnData, (bool))) revert TokenCallFalse();
    }

}
