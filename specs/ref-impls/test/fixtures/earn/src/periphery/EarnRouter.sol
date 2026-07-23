// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IEarnVault } from "../interfaces/core/IEarnVault.sol";
import { IERC20Like } from "../interfaces/external/IERC20Like.sol";
import { ITempoStablecoinDex } from "../interfaces/external/tempo/ITempoStablecoinDex.sol";
import {
    EncryptedDepositPayload,
    IWithdrawalReceiver,
    IZoneFactory,
    IZonePortal,
    ZONE_FACTORY_ADDRESS,
    ZONE_MESSENGER_ADDRESS,
    ZoneInfo
} from "../interfaces/external/tempo/IZone.sol";
import { IERC4626Source } from "../interfaces/periphery/IERC4626Source.sol";
import { ISwapAdapter } from "../interfaces/periphery/ISwapAdapter.sol";

/// @title EarnRouter
/// @notice Ownerless, configuration-free Earn periphery shared by every EarnVault and canonical Zone.
/// @dev The router has no issuer authority and retains no operation-owned funds or approvals after a
///      successful call. Customer-specific swap policy is read from the selected EarnVault.
contract EarnRouter is IWithdrawalReceiver {
    address public constant STABLECOIN_DEX = 0xDEc0000000000000000000000000000000000000;

    enum Flow {
        Deposit,
        Redeem
    }

    enum Destination {
        Zone,
        Public
    }

    struct ZoneDelivery {
        address portal;
        uint256 keyIndex;
        EncryptedDepositPayload encrypted;
        address refundRecipient;
    }

    struct ZoneReturn {
        uint256 keyIndex;
        EncryptedDepositPayload encrypted;
        address refundRecipient;
    }

    /// @notice Callback data for a private-input immediate Earn operation.
    /// @dev `destinationData` encodes an address for Public or `ZoneReturn` for Zone.
    struct CallbackData {
        Flow flow;
        address earnVault;
        Destination destination;
        address outputToken;
        uint128 minVaultAssets;
        uint128 minEarnShares;
        uint128 minOutputAmount;
        bytes32 actionId;
        bytes destinationData;
    }

    uint256 private locked = 1;

    event EarnDeposit(
        bytes32 indexed actionId,
        address indexed earnVault,
        address indexed inputToken,
        uint256 inputAmount,
        uint256 vaultAssets,
        uint256 earnShares,
        bytes32 zoneDepositHash
    );
    event EarnRedeem(
        bytes32 indexed actionId,
        address indexed earnVault,
        address indexed outputToken,
        uint256 earnShares,
        uint256 vaultAssets,
        uint256 outputAmount,
        bytes32 zoneDepositHash
    );
    event PublicDepositRouted(
        address indexed caller, address indexed earnVault, address indexed recipient, uint256 assets, uint256 earnShares
    );
    event PublicRedeemRouted(
        address indexed caller, address indexed earnVault, address indexed recipient, uint256 earnShares, uint256 assets
    );
    event PublicDepositRoutedToZone(
        address indexed caller,
        address indexed earnVault,
        address indexed portal,
        uint256 assets,
        uint256 earnShares,
        bytes32 zoneDepositHash
    );
    event PublicRedeemRoutedToZone(
        address indexed caller,
        address indexed earnVault,
        address indexed portal,
        uint256 earnShares,
        uint256 assets,
        bytes32 zoneDepositHash
    );
    event VaultDepositRouted(
        address indexed caller,
        address indexed earnVault,
        address indexed sourceVault,
        address recipient,
        uint256 sourceVaultShares,
        uint256 assets,
        uint256 earnShares
    );
    event VaultDepositRoutedToZone(
        address indexed caller,
        address indexed earnVault,
        address indexed sourceVault,
        address portal,
        uint256 sourceVaultShares,
        uint256 assets,
        uint256 earnShares,
        bytes32 zoneDepositHash
    );
    event PrivateDepositRoutedToPublic(
        bytes32 indexed actionId,
        address indexed earnVault,
        address indexed recipient,
        address inputToken,
        uint256 inputAmount,
        uint256 vaultAssets,
        uint256 earnShares
    );
    event PrivateRedeemRoutedToPublic(
        bytes32 indexed actionId,
        address indexed earnVault,
        address indexed recipient,
        address outputToken,
        uint256 earnShares,
        uint256 vaultAssets,
        uint256 outputAmount
    );

    error AmountOverflow();
    error BadFlow();
    error InsufficientOutput();
    error InvalidEarnVault();
    error InvalidSourcePortal();
    error InvalidTargetPortal();
    error InvalidToken();
    error NotZoneMessenger();
    error ReentrantCall();
    error ResidualBalance();
    error TokenCallFailed();
    error TokenCallFalse();
    error WrongEarnShare();
    error WrongOutputToken();
    error WrongSourceAsset();
    error ZeroAddress();
    error ZeroAmount();

    modifier nonReentrant() {
        if (locked != 1) revert ReentrantCall();
        locked = 2;
        _;
        locked = 1;
    }

    function deposit(address earnVault, uint256 assets, uint256 minEarnShares, address recipient)
        external
        nonReentrant
        returns (uint256 earnShares)
    {
        (address vaultAsset,) = _earnVaultTokens(earnVault);
        if (recipient == address(0)) revert ZeroAddress();
        if (assets == 0 || minEarnShares == 0) revert ZeroAmount();
        uint256 balanceBefore = IERC20Like(vaultAsset).balanceOf(address(this));

        _safeTransferFrom(vaultAsset, msg.sender, address(this), assets);
        earnShares = _depositHeldAssets(earnVault, vaultAsset, assets, minEarnShares, recipient);
        _requireBalance(vaultAsset, balanceBefore);
        emit PublicDepositRouted(msg.sender, earnVault, recipient, assets, earnShares);
    }

    function depositToZone(address earnVault, uint256 assets, uint256 minEarnShares, ZoneDelivery calldata delivery)
        external
        nonReentrant
        returns (uint256 earnShares, bytes32 zoneDepositHash)
    {
        (address vaultAsset, address earnShare) = _earnVaultTokens(earnVault);
        if (assets == 0 || minEarnShares == 0) revert ZeroAmount();
        _validateZoneDelivery(delivery, earnShare);
        uint256 assetBalanceBefore = IERC20Like(vaultAsset).balanceOf(address(this));
        uint256 earnShareBalanceBefore = IERC20Like(earnShare).balanceOf(address(this));

        _safeTransferFrom(vaultAsset, msg.sender, address(this), assets);
        earnShares = _depositHeldAssets(earnVault, vaultAsset, assets, minEarnShares, address(this));
        zoneDepositHash = _depositEncryptedToZone(earnShare, _toUint128(earnShares), delivery);

        _requireBalance(vaultAsset, assetBalanceBefore);
        _requireBalance(earnShare, earnShareBalanceBefore);
        emit PublicDepositRoutedToZone(msg.sender, earnVault, delivery.portal, assets, earnShares, zoneDepositHash);
    }

    function depositFromVault(
        address earnVault,
        address sourceVault,
        uint256 sourceVaultShares,
        uint256 minRedeemedAssets,
        uint256 minEarnShares,
        address recipient
    ) external nonReentrant returns (uint256 assets, uint256 earnShares) {
        (address vaultAsset,) = _earnVaultTokens(earnVault);
        if (recipient == address(0)) revert ZeroAddress();
        uint256 balanceBefore = IERC20Like(vaultAsset).balanceOf(address(this));
        (assets, earnShares) = _depositFromVault(
            earnVault, vaultAsset, sourceVault, sourceVaultShares, minRedeemedAssets, minEarnShares, recipient
        );
        _requireBalance(vaultAsset, balanceBefore);
        emit VaultDepositRouted(msg.sender, earnVault, sourceVault, recipient, sourceVaultShares, assets, earnShares);
    }

    function depositFromVaultToZone(
        address earnVault,
        address sourceVault,
        uint256 sourceVaultShares,
        uint256 minRedeemedAssets,
        uint256 minEarnShares,
        ZoneDelivery calldata delivery
    ) external nonReentrant returns (uint256 assets, uint256 earnShares, bytes32 zoneDepositHash) {
        (address vaultAsset, address earnShare) = _earnVaultTokens(earnVault);
        _validateZoneDelivery(delivery, earnShare);
        uint256 assetBalanceBefore = IERC20Like(vaultAsset).balanceOf(address(this));
        uint256 earnShareBalanceBefore = IERC20Like(earnShare).balanceOf(address(this));

        (assets, earnShares) = _depositFromVault(
            earnVault, vaultAsset, sourceVault, sourceVaultShares, minRedeemedAssets, minEarnShares, address(this)
        );
        zoneDepositHash = _depositEncryptedToZone(earnShare, _toUint128(earnShares), delivery);

        _requireBalance(vaultAsset, assetBalanceBefore);
        _requireBalance(earnShare, earnShareBalanceBefore);
        emit VaultDepositRoutedToZone(
            msg.sender, earnVault, sourceVault, delivery.portal, sourceVaultShares, assets, earnShares, zoneDepositHash
        );
    }

    function redeem(address earnVault, uint256 earnShares, uint256 minAssets, address recipient)
        external
        nonReentrant
        returns (uint256 assets)
    {
        (address vaultAsset, address earnShare) = _earnVaultTokens(earnVault);
        if (recipient == address(0)) revert ZeroAddress();
        if (earnShares == 0 || minAssets == 0) revert ZeroAmount();
        uint256 earnShareBalanceBefore = IERC20Like(earnShare).balanceOf(address(this));

        _safeTransferFrom(earnShare, msg.sender, address(this), earnShares);
        assets = _redeemHeldEarnShares(earnVault, earnShare, earnShares, minAssets, recipient);

        _requireBalance(earnShare, earnShareBalanceBefore);
        emit PublicRedeemRouted(msg.sender, earnVault, recipient, earnShares, assets);
        vaultAsset;
    }

    function redeemToZone(address earnVault, uint256 earnShares, uint256 minAssets, ZoneDelivery calldata delivery)
        external
        nonReentrant
        returns (uint256 assets, bytes32 zoneDepositHash)
    {
        (address vaultAsset, address earnShare) = _earnVaultTokens(earnVault);
        if (earnShares == 0 || minAssets == 0) revert ZeroAmount();
        _validateZoneDelivery(delivery, vaultAsset);
        uint256 earnShareBalanceBefore = IERC20Like(earnShare).balanceOf(address(this));
        uint256 assetBalanceBefore = IERC20Like(vaultAsset).balanceOf(address(this));

        _safeTransferFrom(earnShare, msg.sender, address(this), earnShares);
        assets = _redeemHeldEarnShares(earnVault, earnShare, earnShares, minAssets, address(this));
        zoneDepositHash = _depositEncryptedToZone(vaultAsset, _toUint128(assets), delivery);

        _requireBalance(earnShare, earnShareBalanceBefore);
        _requireBalance(vaultAsset, assetBalanceBefore);
        emit PublicRedeemRoutedToZone(msg.sender, earnVault, delivery.portal, earnShares, assets, zoneDepositHash);
    }

    function supportsFlow(uint8 flow) external pure returns (bool) {
        return flow <= uint8(Flow.Redeem);
    }

    function onWithdrawalReceived(
        uint32 sourceZoneId,
        address sourcePortal,
        bytes32,
        address token,
        uint128 amount,
        bytes calldata callbackData
    ) external nonReentrant returns (bytes4) {
        _validateWithdrawal(sourceZoneId, sourcePortal, amount);
        uint256 inputBalanceBefore = IERC20Like(token).balanceOf(address(this)) - amount;

        uint256 rawFlow = _decodeRawFlow(callbackData);
        if (rawFlow > uint256(Flow.Redeem)) revert BadFlow();
        CallbackData memory data = abi.decode(callbackData, (CallbackData));
        _dispatchRouterWithdrawal(sourcePortal, Flow(rawFlow), token, amount, data);

        _requireBalance(token, inputBalanceBefore);
        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }

    function _depositFromVault(
        address earnVault,
        address vaultAsset,
        address sourceVault,
        uint256 sourceVaultShares,
        uint256 minRedeemedAssets,
        uint256 minEarnShares,
        address recipient
    ) internal returns (uint256 assets, uint256 earnShares) {
        if (sourceVault == address(0) || recipient == address(0)) revert ZeroAddress();
        if (sourceVaultShares == 0 || minRedeemedAssets == 0 || minEarnShares == 0) revert ZeroAmount();
        if (IERC4626Source(sourceVault).asset() != vaultAsset) revert WrongSourceAsset();

        uint256 beforeBalance = IERC20Like(vaultAsset).balanceOf(address(this));
        IERC4626Source(sourceVault).redeem(sourceVaultShares, address(this), msg.sender);
        uint256 afterBalance = IERC20Like(vaultAsset).balanceOf(address(this));
        if (afterBalance <= beforeBalance) revert InsufficientOutput();
        assets = afterBalance - beforeBalance;
        if (assets < minRedeemedAssets) revert InsufficientOutput();
        earnShares = _depositHeldAssets(earnVault, vaultAsset, assets, minEarnShares, recipient);
    }

    function _depositHeldAssets(
        address earnVault,
        address vaultAsset,
        uint256 assets,
        uint256 minEarnShares,
        address recipient
    ) internal returns (uint256 earnShares) {
        _safeApprove(vaultAsset, earnVault, assets);
        earnShares = IEarnVault(earnVault).deposit(assets, recipient, minEarnShares);
        _safeApprove(vaultAsset, earnVault, 0);
        if (earnShares == 0 || earnShares < minEarnShares) revert InsufficientOutput();
    }

    function _redeemHeldEarnShares(
        address earnVault,
        address earnShare,
        uint256 earnShares,
        uint256 minAssets,
        address recipient
    ) internal returns (uint256 assets) {
        _safeApprove(earnShare, earnVault, earnShares);
        assets = IEarnVault(earnVault).redeem(earnShares, recipient, minAssets);
        _safeApprove(earnShare, earnVault, 0);
        if (assets == 0 || assets < minAssets) revert InsufficientOutput();
    }

    function _dispatchRouterWithdrawal(
        address sourcePortal,
        Flow flow,
        address token,
        uint128 amount,
        CallbackData memory data
    ) internal {
        (address vaultAsset, address earnShare) = _earnVaultTokens(data.earnVault);
        if (data.destination == Destination.Public) {
            address recipient = abi.decode(data.destinationData, (address));
            if (recipient == address(0)) revert ZeroAddress();
            if (flow == Flow.Deposit) {
                _handleDepositToPublic(data.earnVault, vaultAsset, earnShare, token, amount, recipient, data);
            } else {
                _handleRedeemToPublic(data.earnVault, vaultAsset, earnShare, token, amount, recipient, data);
            }
        } else if (data.destination == Destination.Zone) {
            ZoneReturn memory destination = abi.decode(data.destinationData, (ZoneReturn));
            if (flow == Flow.Deposit) {
                _handleDepositToZone(
                    sourcePortal, data.earnVault, vaultAsset, earnShare, token, amount, destination, data
                );
            } else {
                _handleRedeemToZone(
                    sourcePortal, data.earnVault, vaultAsset, earnShare, token, amount, destination, data
                );
            }
        } else {
            revert BadFlow();
        }
    }

    function _handleDepositToPublic(
        address earnVault,
        address vaultAsset,
        address earnShare,
        address token,
        uint128 amount,
        address recipient,
        CallbackData memory data
    ) internal {
        uint256 vaultAssets = _prepareDeposit(earnVault, vaultAsset, earnShare, token, amount, data);
        uint256 earnShares = _depositHeldAssets(earnVault, vaultAsset, vaultAssets, data.minEarnShares, recipient);
        emit PrivateDepositRoutedToPublic(data.actionId, earnVault, recipient, token, amount, vaultAssets, earnShares);
    }

    function _handleDepositToZone(
        address sourcePortal,
        address earnVault,
        address vaultAsset,
        address earnShare,
        address token,
        uint128 amount,
        ZoneReturn memory destination,
        CallbackData memory data
    ) internal {
        uint256 earnShareBalanceBefore = IERC20Like(earnShare).balanceOf(address(this));
        uint256 vaultAssets = _prepareDeposit(earnVault, vaultAsset, earnShare, token, amount, data);
        uint256 earnShares = _depositHeldAssets(earnVault, vaultAsset, vaultAssets, data.minEarnShares, address(this));
        bytes32 zoneDepositHash =
            _returnEncryptedToSourceZone(sourcePortal, earnShare, _toUint128(earnShares), destination);
        _requireBalance(earnShare, earnShareBalanceBefore);
        emit EarnDeposit(data.actionId, earnVault, token, amount, vaultAssets, earnShares, zoneDepositHash);
    }

    function _handleRedeemToPublic(
        address earnVault,
        address vaultAsset,
        address earnShare,
        address token,
        uint128 amount,
        address recipient,
        CallbackData memory data
    ) internal {
        (uint256 vaultAssets, uint256 outputAmount) =
            _prepareRedeem(earnVault, vaultAsset, earnShare, token, amount, data);
        _safeTransfer(data.outputToken, recipient, outputAmount);
        emit PrivateRedeemRoutedToPublic(
            data.actionId, earnVault, recipient, data.outputToken, amount, vaultAssets, outputAmount
        );
    }

    function _handleRedeemToZone(
        address sourcePortal,
        address earnVault,
        address vaultAsset,
        address earnShare,
        address token,
        uint128 amount,
        ZoneReturn memory destination,
        CallbackData memory data
    ) internal {
        uint256 outputBalanceBefore = IERC20Like(data.outputToken).balanceOf(address(this));
        (uint256 vaultAssets, uint256 outputAmount) =
            _prepareRedeem(earnVault, vaultAsset, earnShare, token, amount, data);
        bytes32 zoneDepositHash =
            _returnEncryptedToSourceZone(sourcePortal, data.outputToken, _toUint128(outputAmount), destination);
        _requireBalance(data.outputToken, outputBalanceBefore);
        emit EarnRedeem(data.actionId, earnVault, data.outputToken, amount, vaultAssets, outputAmount, zoneDepositHash);
    }

    function _prepareDeposit(
        address earnVault,
        address vaultAsset,
        address earnShare,
        address token,
        uint128 amount,
        CallbackData memory data
    ) internal returns (uint256 vaultAssets) {
        if (data.outputToken != earnShare) revert WrongOutputToken();
        if (token == vaultAsset) {
            vaultAssets = amount;
        } else {
            vaultAssets = _swap(
                IEarnVault(earnVault).depositSwapOverride(token), token, vaultAsset, amount, data.minVaultAssets
            );
        }
        if (vaultAssets == 0 || vaultAssets < data.minVaultAssets) revert InsufficientOutput();
    }

    function _prepareRedeem(
        address earnVault,
        address vaultAsset,
        address earnShare,
        address token,
        uint128 amount,
        CallbackData memory data
    ) internal returns (uint256 vaultAssets, uint256 outputAmount) {
        if (token != earnShare) revert WrongEarnShare();
        if (data.outputToken == address(0) || data.outputToken == earnShare) revert WrongOutputToken();

        vaultAssets = _redeemHeldEarnShares(earnVault, earnShare, amount, data.minVaultAssets, address(this));
        outputAmount = vaultAssets;
        if (data.outputToken != vaultAsset) {
            outputAmount = _swap(
                IEarnVault(earnVault).redeemSwapOverride(data.outputToken),
                vaultAsset,
                data.outputToken,
                vaultAssets,
                data.minOutputAmount
            );
        }
        if (outputAmount == 0 || outputAmount < data.minOutputAmount) revert InsufficientOutput();
    }

    function _swap(address overrideAdapter, address tokenIn, address tokenOut, uint256 amountIn, uint256 minAmountOut)
        internal
        returns (uint256 amountOut)
    {
        uint256 inputBalance = IERC20Like(tokenIn).balanceOf(address(this));
        if (inputBalance < amountIn) revert InsufficientOutput();
        uint256 inputBalanceBefore = inputBalance - amountIn;
        uint256 outputBalanceBefore = IERC20Like(tokenOut).balanceOf(address(this));
        address spender = overrideAdapter == address(0) ? STABLECOIN_DEX : overrideAdapter;

        _safeApprove(tokenIn, spender, amountIn);
        if (overrideAdapter == address(0)) {
            ITempoStablecoinDex(STABLECOIN_DEX)
                .swapExactAmountIn(tokenIn, tokenOut, _toUint128(amountIn), _toUint128(minAmountOut));
        } else {
            ISwapAdapter(overrideAdapter).swapExactIn(tokenIn, tokenOut, amountIn, address(this), minAmountOut);
        }
        _safeApprove(tokenIn, spender, 0);

        uint256 outputBalanceAfter = IERC20Like(tokenOut).balanceOf(address(this));
        if (outputBalanceAfter <= outputBalanceBefore) revert InsufficientOutput();
        amountOut = outputBalanceAfter - outputBalanceBefore;
        if (amountOut < minAmountOut) revert InsufficientOutput();
        _requireBalance(tokenIn, inputBalanceBefore);
    }

    function _validateWithdrawal(uint32 sourceZoneId, address sourcePortal, uint128 amount) internal view {
        if (msg.sender != ZONE_MESSENGER_ADDRESS) revert NotZoneMessenger();
        ZoneInfo memory zone = IZoneFactory(ZONE_FACTORY_ADDRESS).zones(sourceZoneId);
        if (zone.portal != sourcePortal || !IZoneFactory(ZONE_FACTORY_ADDRESS).isZonePortal(sourcePortal)) {
            revert InvalidSourcePortal();
        }
        if (amount == 0) revert ZeroAmount();
    }

    function _validateZoneDelivery(ZoneDelivery memory delivery, address token) internal view {
        if (delivery.portal == address(0) || delivery.refundRecipient == address(0)) revert ZeroAddress();
        _validateTargetPortal(delivery.portal, token);
    }

    function _validateTargetPortal(address portal, address token) internal view {
        if (!IZoneFactory(ZONE_FACTORY_ADDRESS).isZonePortal(portal)) revert InvalidTargetPortal();
        if (!IZonePortal(portal).isTokenEnabled(token) || !IZonePortal(portal).areDepositsActive(token)) {
            revert InvalidToken();
        }
    }

    function _depositEncryptedToZone(address token, uint128 amount, ZoneDelivery memory delivery)
        internal
        returns (bytes32 hash)
    {
        _safeApprove(token, delivery.portal, amount);
        hash = IZonePortal(delivery.portal)
            .depositEncrypted(token, amount, delivery.keyIndex, delivery.encrypted, delivery.refundRecipient);
        _safeApprove(token, delivery.portal, 0);
    }

    function _returnEncryptedToSourceZone(
        address sourcePortal,
        address token,
        uint128 amount,
        ZoneReturn memory destination
    ) internal returns (bytes32 hash) {
        if (destination.refundRecipient == address(0)) revert ZeroAddress();
        _validateTargetPortal(sourcePortal, token);
        _safeApprove(token, sourcePortal, amount);
        hash = IZonePortal(sourcePortal)
            .depositEncrypted(token, amount, destination.keyIndex, destination.encrypted, destination.refundRecipient);
        _safeApprove(token, sourcePortal, 0);
    }

    function _earnVaultTokens(address earnVault) internal view returns (address vaultAsset, address earnShare) {
        if (earnVault == address(0) || earnVault.code.length == 0) revert InvalidEarnVault();
        vaultAsset = IEarnVault(earnVault).asset();
        earnShare = IEarnVault(earnVault).earnShare();
        if (vaultAsset == address(0) || earnShare == address(0)) revert InvalidEarnVault();
    }

    function _decodeRawFlow(bytes calldata callbackData) internal pure returns (uint256 rawFlow) {
        if (callbackData.length < 32) revert BadFlow();
        uint256 tupleOffset = abi.decode(callbackData, (uint256));
        if (tupleOffset > callbackData.length || callbackData.length - tupleOffset < 32) revert BadFlow();
        assembly {
            rawFlow := calldataload(add(callbackData.offset, tupleOffset))
        }
    }

    function _requireBalance(address token, uint256 expected) internal view {
        if (IERC20Like(token).balanceOf(address(this)) != expected) revert ResidualBalance();
    }

    function _toUint128(uint256 value) internal pure returns (uint128) {
        if (value > type(uint128).max) revert AmountOverflow();
        // The explicit bound above makes this narrowing conversion lossless.
        // forge-lint: disable-next-line(unsafe-typecast)
        return uint128(value);
    }

    function _safeApprove(address token, address spender, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeCall(IERC20Like.approve, (spender, 0)));
        if (value != 0) _callOptionalReturn(token, abi.encodeCall(IERC20Like.approve, (spender, value)));
    }

    function _safeTransfer(address token, address to, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeCall(IERC20Like.transfer, (to, value)));
    }

    function _safeTransferFrom(address token, address from, address to, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeCall(IERC20Like.transferFrom, (from, to, value)));
    }

    function _callOptionalReturn(address token, bytes memory data) private {
        (bool ok, bytes memory returnData) = token.call(data);
        if (!ok) revert TokenCallFailed();
        if (returnData.length != 0 && !abi.decode(returnData, (bool))) revert TokenCallFalse();
    }
}
