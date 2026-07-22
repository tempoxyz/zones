// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC20Like } from "../interfaces/IERC20Like.sol";
import { IStableSwap } from "../interfaces/IStableSwap.sol";
import { IVaultAdapter } from "../interfaces/IVaultAdapter.sol";
import { IZonePortal } from "../interfaces/IZonePortal.sol";
import { Ownable } from "@openzeppelin/contracts/access/Ownable.sol";
import { Ownable2Step } from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice Shared Zone authentication, routing, ownership, and token plumbing.
/// @dev Capability-specific gateways define callback flows and custody rules on top of this base.
abstract contract ZoneGatewayBase is Ownable2Step {

    address public immutable vaultAdapter;
    address public immutable vaultAsset;
    address public immutable shareToken;
    address public immutable defaultSwapper;
    address public immutable zonePortal;
    address public immutable zoneMessenger;
    uint32 public immutable zoneId;

    mapping(address token => address swapper) public depositSwapperFor;
    mapping(address token => address swapper) public redeemSwapperFor;

    uint256 private locked = 1;

    event DepositRouteUpdated(address indexed inputToken, address indexed swapper);
    event RedeemRouteUpdated(address indexed outputToken, address indexed swapper);
    event TokenRescued(address indexed token, address indexed receiver, uint256 amount);

    error AmountOverflow();
    error BadFlow();
    error BadRouteConfig();
    error InsufficientOutput();
    error InvalidSourcePortal();
    error InvalidZoneConfiguration();
    error NotZoneMessenger();
    error ReentrantCall();
    error TokenCallFailed();
    error TokenCallFalse();
    error WrongOutputToken();
    error WrongShareToken();
    error ZeroAddress();
    error ZeroAmount();

    constructor(
        address vaultAdapter_,
        address defaultSwapper_,
        address zonePortal_,
        address zoneMessenger_,
        address owner_
    )
        Ownable(owner_)
    {
        if (
            vaultAdapter_ == address(0) || defaultSwapper_ == address(0)
                || zonePortal_ == address(0) || zoneMessenger_ == address(0) || owner_ == address(0)
        ) {
            revert ZeroAddress();
        }

        address vaultAsset_ = IVaultAdapter(vaultAdapter_).asset();
        address shareToken_ = IVaultAdapter(vaultAdapter_).shareToken();
        if (vaultAsset_ == address(0) || shareToken_ == address(0)) revert ZeroAddress();

        IZonePortal portal = IZonePortal(zonePortal_);
        uint32 zoneId_ = portal.zoneId();
        if (portal.messenger() != zoneMessenger_) revert InvalidZoneConfiguration();

        vaultAdapter = vaultAdapter_;
        vaultAsset = vaultAsset_;
        shareToken = shareToken_;
        defaultSwapper = defaultSwapper_;
        zonePortal = zonePortal_;
        zoneMessenger = zoneMessenger_;
        zoneId = zoneId_;
    }

    modifier nonReentrant() {
        if (locked != 1) revert ReentrantCall();
        locked = 2;
        _;
        locked = 1;
    }

    /// @notice Sets the swapper used to convert `inputToken` into the vault asset on deposit.
    /// @dev A zero `swapper` clears the override so the token falls back to `defaultSwapper`.
    function setDepositRoute(address inputToken, address swapper) external onlyOwner {
        if (inputToken == address(0) || inputToken == vaultAsset || inputToken == shareToken) {
            revert BadRouteConfig();
        }
        depositSwapperFor[inputToken] = swapper;
        emit DepositRouteUpdated(inputToken, swapper);
    }

    /// @notice Sets the swapper used to convert the vault asset into `outputToken` on redeem.
    /// @dev A zero `swapper` clears the override so the token falls back to `defaultSwapper`.
    function setRedeemRoute(address outputToken, address swapper) external onlyOwner {
        if (outputToken == address(0) || outputToken == vaultAsset || outputToken == shareToken) {
            revert BadRouteConfig();
        }
        redeemSwapperFor[outputToken] = swapper;
        emit RedeemRouteUpdated(outputToken, swapper);
    }

    function _validateWithdrawal(
        uint32 sourceZoneId,
        address sourcePortal,
        uint128 amount
    )
        internal
        view
    {
        if (sourcePortal != zonePortal || sourceZoneId != zoneId) revert InvalidSourcePortal();
        if (msg.sender != zoneMessenger) revert NotZoneMessenger();
        if (amount == 0) revert ZeroAmount();
    }

    function _swap(
        address swapper,
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut
    )
        internal
        returns (uint256 amountOut)
    {
        uint256 beforeBalance = IERC20Like(tokenOut).balanceOf(address(this));
        _safeApprove(tokenIn, swapper, 0);
        _safeApprove(tokenIn, swapper, amountIn);
        // The observed output-token delta is authoritative, not the swapper's return value.
        // forge-lint: disable-next-line(unused-return)
        IStableSwap(swapper).swapExactIn(tokenIn, tokenOut, amountIn, address(this), minAmountOut);
        uint256 afterBalance = IERC20Like(tokenOut).balanceOf(address(this));
        if (afterBalance < beforeBalance) revert InsufficientOutput();
        amountOut = afterBalance - beforeBalance;
        if (amountOut == 0 || amountOut < minAmountOut) revert InsufficientOutput();
    }

    function _depositSwapper(address token) internal view returns (address) {
        address swapper = depositSwapperFor[token];
        return swapper == address(0) ? defaultSwapper : swapper;
    }

    function _redeemSwapper(address token) internal view returns (address) {
        address swapper = redeemSwapperFor[token];
        return swapper == address(0) ? defaultSwapper : swapper;
    }

    function _decodeRawFlow(bytes calldata callbackData) internal pure returns (uint256 rawFlow) {
        if (callbackData.length < 32) revert BadFlow();

        uint256 tupleOffset = abi.decode(callbackData, (uint256));
        if (tupleOffset > callbackData.length || callbackData.length - tupleOffset < 32) {
            revert BadFlow();
        }

        assembly {
            rawFlow := calldataload(add(callbackData.offset, tupleOffset))
        }
    }

    function _toUint128(uint256 value) internal pure returns (uint128) {
        if (value > type(uint128).max) revert AmountOverflow();
        // forge-lint: disable-next-line(unsafe-typecast)
        return uint128(value);
    }

    function _safeApprove(address token, address spender, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeCall(IERC20Like.approve, (spender, value)));
    }

    function _safeTransfer(address token, address to, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeCall(IERC20Like.transfer, (to, value)));
    }

    function _callOptionalReturn(address token, bytes memory data) private {
        (bool ok, bytes memory returnData) = token.call(data);
        if (!ok) revert TokenCallFailed();
        if (returnData.length != 0 && !abi.decode(returnData, (bool))) revert TokenCallFalse();
    }

}
