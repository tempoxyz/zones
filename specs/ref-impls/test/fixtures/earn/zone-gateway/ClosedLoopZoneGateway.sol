// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC20Like } from "../interfaces/IERC20Like.sol";
import { IStableSwap } from "../interfaces/IStableSwap.sol";
import { IVaultAdapter } from "../interfaces/IVaultAdapter.sol";
import { IZonePortal } from "../interfaces/IZonePortal.sol";
import { IZoneWithdrawalReceiver } from "../interfaces/IZoneWithdrawalReceiver.sol";

/// @notice Callback-only gateway for synchronous, closed-loop Earn flows.
/// @dev Every successful callback deposits its complete output back into the originating Zone.
///      If the vault operation, swap, or encrypted return fails, the callback reverts and the
///      Zone protocol handles the original withdrawal through its private bounce-back path.
contract ClosedLoopZoneGateway is IZoneWithdrawalReceiver {

    enum Flow {
        Deposit,
        Redeem
    }

    struct CallbackData {
        Flow flow;
        address outputToken;
        uint256 keyIndex;
        IZonePortal.EncryptedDepositPayload encrypted;
        uint128 minVaultAssets;
        uint128 minVaultShares;
        uint128 minOutputAmount;
        bytes32 actionId;
        address refundRecipient;
    }

    address public immutable vaultAdapter;
    address public immutable vaultAsset;
    address public immutable shareToken;
    address public immutable defaultSwapper;
    address public immutable zonePortal;
    address public immutable zoneMessenger;
    uint32 public immutable zoneId;

    address public owner;
    address public pendingOwner;

    mapping(address token => address swapper) public depositSwapperFor;
    mapping(address token => address swapper) public redeemSwapperFor;

    uint256 private locked = 1;

    event OwnershipTransferStarted(address indexed previousOwner, address indexed pendingOwner);
    event OwnerUpdated(address indexed owner);
    event DepositRouteUpdated(address indexed inputToken, address indexed swapper);
    event RedeemRouteUpdated(address indexed outputToken, address indexed swapper);
    event EarnDeposit(
        bytes32 indexed actionId,
        address indexed inputToken,
        uint256 inputAmount,
        uint256 vaultAssets,
        uint256 shares,
        bytes32 zoneDepositHash
    );
    event EarnRedeem(
        bytes32 indexed actionId,
        address indexed outputToken,
        uint256 shares,
        uint256 vaultAssets,
        uint256 outputAmount,
        bytes32 zoneDepositHash
    );
    event TokenRescued(address indexed token, address indexed receiver, uint256 amount);

    error AmountOverflow();
    error BadFlow();
    error BadRouteConfig();
    error InsufficientOutput();
    error InvalidSourcePortal();
    error InvalidZoneConfiguration();
    error NotOwner();
    error NotPendingOwner();
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
    ) {
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
        owner = owner_;

        emit OwnerUpdated(owner_);
    }

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier nonReentrant() {
        if (locked != 1) revert ReentrantCall();
        locked = 2;
        _;
        locked = 1;
    }

    /// @notice Returns true only for the two flows that satisfy the closed-loop Zone invariant.
    function supportsFlow(uint8 flow) external pure returns (bool) {
        return flow <= uint8(Flow.Redeem);
    }

    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        pendingOwner = newOwner;
        emit OwnershipTransferStarted(owner, newOwner);
    }

    function acceptOwnership() external {
        if (msg.sender != pendingOwner) revert NotPendingOwner();
        owner = msg.sender;
        pendingOwner = address(0);
        emit OwnerUpdated(msg.sender);
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

    /// @inheritdoc IZoneWithdrawalReceiver
    function onWithdrawalReceived(
        uint32 sourceZoneId,
        address sourcePortal,
        bytes32,
        address token,
        uint128 amount,
        bytes calldata callbackData
    )
        external
        nonReentrant
        returns (bytes4)
    {
        if (sourcePortal != zonePortal || sourceZoneId != zoneId) revert InvalidSourcePortal();
        if (msg.sender != zoneMessenger) revert NotZoneMessenger();
        if (amount == 0) revert ZeroAmount();

        Flow flow = _decodeFlow(callbackData);
        CallbackData memory data = abi.decode(callbackData, (CallbackData));
        if (data.refundRecipient == address(0)) revert ZeroAddress();

        if (flow == Flow.Deposit) {
            _handleDeposit(token, amount, data);
        } else if (flow == Flow.Redeem) {
            _handleRedeem(token, amount, data);
        } else {
            revert BadFlow();
        }

        return IZoneWithdrawalReceiver.onWithdrawalReceived.selector;
    }

    /// @notice Recovers tokens accidentally sent outside a callback.
    /// @dev Closed-loop callbacks never retain a legitimate balance: every success returns the
    ///      complete output to the Zone and every failure reverts atomically.
    function rescueToken(
        address token,
        address receiver,
        uint256 amount
    )
        external
        onlyOwner
        nonReentrant
    {
        if (token == address(0) || receiver == address(0)) revert ZeroAddress();
        _safeTransfer(token, receiver, amount);
        emit TokenRescued(token, receiver, amount);
    }

    function _handleDeposit(address token, uint128 amount, CallbackData memory data) internal {
        if (data.outputToken != shareToken) revert WrongOutputToken();

        uint256 vaultAssets_;
        if (token == vaultAsset) {
            vaultAssets_ = amount;
        } else {
            vaultAssets_ =
                _swap(_depositSwapper(token), token, vaultAsset, amount, data.minVaultAssets);
        }
        if (vaultAssets_ == 0 || vaultAssets_ < data.minVaultAssets) revert InsufficientOutput();

        _safeApprove(vaultAsset, vaultAdapter, 0);
        _safeApprove(vaultAsset, vaultAdapter, vaultAssets_);
        uint256 shares =
            IVaultAdapter(vaultAdapter).deposit(vaultAssets_, address(this), data.minVaultShares);
        if (shares == 0 || shares < data.minVaultShares) revert InsufficientOutput();

        bytes32 zoneDepositHash = _returnToZone(shareToken, _toUint128(shares), data);
        emit EarnDeposit(data.actionId, token, amount, vaultAssets_, shares, zoneDepositHash);
    }

    function _handleRedeem(address token, uint128 amount, CallbackData memory data) internal {
        if (token != shareToken) revert WrongShareToken();
        if (data.outputToken == address(0) || data.outputToken == shareToken) {
            revert WrongOutputToken();
        }

        _safeApprove(shareToken, vaultAdapter, 0);
        _safeApprove(shareToken, vaultAdapter, amount);
        uint256 vaultAssets_ = IVaultAdapter(vaultAdapter).redeem(amount, address(this));
        if (vaultAssets_ == 0 || vaultAssets_ < data.minVaultAssets) revert InsufficientOutput();

        uint256 outputAmount = vaultAssets_;
        if (data.outputToken != vaultAsset) {
            outputAmount = _swap(
                _redeemSwapper(data.outputToken),
                vaultAsset,
                data.outputToken,
                vaultAssets_,
                data.minOutputAmount
            );
        }
        if (outputAmount == 0 || outputAmount < data.minOutputAmount) revert InsufficientOutput();

        bytes32 zoneDepositHash = _returnToZone(data.outputToken, _toUint128(outputAmount), data);
        emit EarnRedeem(
            data.actionId, data.outputToken, amount, vaultAssets_, outputAmount, zoneDepositHash
        );
    }

    function _returnToZone(
        address token,
        uint128 amount,
        CallbackData memory data
    )
        internal
        returns (bytes32 hash)
    {
        _safeApprove(token, zonePortal, 0);
        _safeApprove(token, zonePortal, amount);
        hash = IZonePortal(zonePortal)
            .depositEncrypted(token, amount, data.keyIndex, data.encrypted, data.refundRecipient);
        _safeApprove(token, zonePortal, 0);
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

    function _decodeFlow(bytes calldata callbackData) internal pure returns (Flow flow) {
        if (callbackData.length < 32) revert BadFlow();

        uint256 tupleOffset = abi.decode(callbackData, (uint256));
        if (tupleOffset > callbackData.length || callbackData.length - tupleOffset < 32) {
            revert BadFlow();
        }

        uint256 rawFlow = 0;
        assembly {
            rawFlow := calldataload(add(callbackData.offset, tupleOffset))
        }
        if (rawFlow > uint256(Flow.Redeem)) revert BadFlow();
        flow = Flow(rawFlow);
    }

    function _toUint128(uint256 value) internal pure returns (uint128) {
        if (value > type(uint128).max) revert AmountOverflow();
        // forge-lint: disable-next-line(unsafe-typecast)
        return uint128(value);
    }

    function _safeApprove(address token, address spender, uint256 value) internal {
        (bool ok, bytes memory data) =
            token.call(abi.encodeCall(IERC20Like.approve, (spender, value)));
        if (!ok) revert TokenCallFailed();
        if (data.length != 0 && !abi.decode(data, (bool))) revert TokenCallFalse();
    }

    function _safeTransfer(address token, address to, uint256 value) internal {
        (bool ok, bytes memory data) = token.call(abi.encodeCall(IERC20Like.transfer, (to, value)));
        if (!ok) revert TokenCallFailed();
        if (data.length != 0 && !abi.decode(data, (bool))) revert TokenCallFalse();
    }

}
