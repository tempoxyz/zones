// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import { IERC20Like } from "../interfaces/external/IERC20Like.sol";
import { ISwapAdapter } from "../interfaces/periphery/ISwapAdapter.sol";

interface ITokenAuthority {
    function unwrap(address stablecoin, uint256 amount) external;
    function wrap(address stablecoin, address receiver, uint256 amount) external;
}

/// @notice Minimal 1:1 swap override for two stablecoins backed by one reserve token.
/// @dev Only the immutable EarnRouter may call this adapter. Controller role revocation
///      and EarnVault override rotation provide the emergency response path.
contract MinimalDirectSwapAdapter is ISwapAdapter {
    address public immutable earnRouter;
    address public immutable tokenAuthority;
    address public immutable reserveToken;
    address public immutable tokenA;
    address public immutable tokenB;

    error InsufficientOutput();
    error InvalidToken();
    error NotEarnRouter();
    error SameToken();
    error TokenCallFailed();
    error TokenCallFalse();
    error ZeroAddress();
    error ZeroAmount();

    constructor(address earnRouter_, address tokenAuthority_, address reserveToken_, address tokenA_, address tokenB_) {
        if (
            earnRouter_ == address(0) || tokenAuthority_ == address(0) || reserveToken_ == address(0)
                || tokenA_ == address(0) || tokenB_ == address(0)
        ) revert ZeroAddress();
        if (tokenA_ == tokenB_) revert SameToken();

        earnRouter = earnRouter_;
        tokenAuthority = tokenAuthority_;
        reserveToken = reserveToken_;
        tokenA = tokenA_;
        tokenB = tokenB_;

        _safeApprove(tokenA_, tokenAuthority_, type(uint256).max);
        _safeApprove(tokenB_, tokenAuthority_, type(uint256).max);
        _safeApprove(reserveToken_, tokenAuthority_, type(uint256).max);
    }

    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn, address receiver, uint256 minAmountOut)
        external
        returns (uint256 amountOut)
    {
        if (msg.sender != earnRouter) revert NotEarnRouter();
        if (amountIn == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        if (minAmountOut > amountIn) revert InsufficientOutput();
        _validatePair(tokenIn, tokenOut);

        _safeTransferFrom(tokenIn, msg.sender, address(this), amountIn);
        ITokenAuthority(tokenAuthority).unwrap(tokenIn, amountIn);
        ITokenAuthority(tokenAuthority).wrap(tokenOut, receiver, amountIn);
        return amountIn;
    }

    function _validatePair(address tokenIn, address tokenOut) internal view {
        if (tokenIn == tokenA && tokenOut == tokenB) return;
        if (tokenIn == tokenB && tokenOut == tokenA) return;
        revert InvalidToken();
    }

    function _safeApprove(address token, address spender, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeWithSelector(IERC20Like.approve.selector, spender, value));
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
