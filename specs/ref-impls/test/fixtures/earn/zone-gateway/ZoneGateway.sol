// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IVaultAdapter } from "../interfaces/IVaultAdapter.sol";
import { IZonePortal } from "../interfaces/IZonePortal.sol";
import { IZoneWithdrawalReceiver } from "../interfaces/IZoneWithdrawalReceiver.sol";
import { ZoneGatewayBase } from "./ZoneGatewayBase.sol";

/// @notice Default gateway for synchronous, closed-loop Earn flows.
/// @dev Every successful callback deposits its complete output back into the originating Zone.
///      If the vault operation, swap, or encrypted return fails, the callback reverts and the
///      Zone protocol handles the original withdrawal through its private bounce-back path.
contract ZoneGateway is ZoneGatewayBase, IZoneWithdrawalReceiver {
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

    constructor(
        address vaultAdapter_,
        address defaultSwapper_,
        address zonePortal_,
        address zoneMessenger_,
        address owner_
    ) ZoneGatewayBase(vaultAdapter_, defaultSwapper_, zonePortal_, zoneMessenger_, owner_) { }

    /// @notice Returns true only for the two flows that satisfy the closed-loop Zone invariant.
    function supportsFlow(uint8 flow) public pure virtual returns (bool) {
        return flow <= uint8(Flow.Redeem);
    }

    /// @inheritdoc IZoneWithdrawalReceiver
    function onWithdrawalReceived(
        uint32 sourceZoneId,
        address sourcePortal,
        bytes32,
        address token,
        uint128 amount,
        bytes calldata callbackData
    ) public virtual nonReentrant returns (bytes4) {
        _validateWithdrawal(sourceZoneId, sourcePortal, amount);

        uint256 rawFlow = _decodeRawFlow(callbackData);
        if (rawFlow > uint256(Flow.Redeem)) revert BadFlow();

        CallbackData memory data = abi.decode(callbackData, (CallbackData));
        if (data.refundRecipient == address(0)) revert ZeroAddress();
        _dispatchSyncWithdrawal(Flow(rawFlow), token, amount, data);

        return IZoneWithdrawalReceiver.onWithdrawalReceived.selector;
    }

    /// @notice Recovers tokens accidentally sent outside a callback.
    /// @dev Synchronous callbacks never retain a legitimate balance: every success returns the
    ///      complete output to the Zone and every failure reverts atomically.
    function rescueToken(address token, address receiver, uint256 amount) public virtual onlyOwner nonReentrant {
        if (token == address(0) || receiver == address(0)) revert ZeroAddress();
        _safeTransfer(token, receiver, amount);
        emit TokenRescued(token, receiver, amount);
    }

    function _dispatchSyncWithdrawal(Flow flow, address token, uint128 amount, CallbackData memory data) internal {
        if (flow == Flow.Deposit) {
            _handleDeposit(token, amount, data);
        } else if (flow == Flow.Redeem) {
            _handleRedeem(token, amount, data);
        } else {
            revert BadFlow();
        }
    }

    function _handleDeposit(address token, uint128 amount, CallbackData memory data) internal {
        if (data.outputToken != shareToken) revert WrongOutputToken();

        uint256 vaultAssets_;
        if (token == vaultAsset) {
            vaultAssets_ = amount;
        } else {
            vaultAssets_ = _swap(_depositSwapper(token), token, vaultAsset, amount, data.minVaultAssets);
        }
        if (vaultAssets_ == 0 || vaultAssets_ < data.minVaultAssets) revert InsufficientOutput();

        _safeApprove(vaultAsset, vaultAdapter, 0);
        _safeApprove(vaultAsset, vaultAdapter, vaultAssets_);
        uint256 shares = IVaultAdapter(vaultAdapter).deposit(vaultAssets_, address(this), data.minVaultShares);
        if (shares == 0 || shares < data.minVaultShares) revert InsufficientOutput();

        bytes32 zoneDepositHash = _returnToZone(shareToken, _toUint128(shares), data);
        emit EarnDeposit(data.actionId, token, amount, vaultAssets_, shares, zoneDepositHash);
    }

    function _handleRedeem(address token, uint128 amount, CallbackData memory data) internal {
        if (token != shareToken) revert WrongShareToken();
        if (data.outputToken == address(0) || data.outputToken == shareToken) revert WrongOutputToken();

        _safeApprove(shareToken, vaultAdapter, 0);
        _safeApprove(shareToken, vaultAdapter, amount);
        uint256 vaultAssets_ = IVaultAdapter(vaultAdapter).redeem(amount, address(this), data.minVaultAssets);
        if (vaultAssets_ == 0 || vaultAssets_ < data.minVaultAssets) revert InsufficientOutput();

        uint256 outputAmount = vaultAssets_;
        if (data.outputToken != vaultAsset) {
            outputAmount = _swap(
                _redeemSwapper(data.outputToken), vaultAsset, data.outputToken, vaultAssets_, data.minOutputAmount
            );
        }
        if (outputAmount == 0 || outputAmount < data.minOutputAmount) revert InsufficientOutput();

        bytes32 zoneDepositHash = _returnToZone(data.outputToken, _toUint128(outputAmount), data);
        emit EarnRedeem(data.actionId, data.outputToken, amount, vaultAssets_, outputAmount, zoneDepositHash);
    }

    function _returnToZone(address token, uint128 amount, CallbackData memory data) internal returns (bytes32 hash) {
        _safeApprove(token, zonePortal, 0);
        _safeApprove(token, zonePortal, amount);
        hash = IZonePortal(zonePortal)
            .depositEncrypted(token, amount, data.keyIndex, data.encrypted, data.refundRecipient);
        _safeApprove(token, zonePortal, 0);
    }
}
