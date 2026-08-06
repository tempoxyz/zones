// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    DepositPayload,
    IWithdrawalReceiver,
    IZonePortal,
    Role
} from "../../src/interfaces/IZone.sol";
import { EncryptedDepositLib } from "../../src/libraries/EncryptedDeposit.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";

enum GatewayFlow {
    Deposit,
    Redeem
}

struct GatewayCallbackData {
    GatewayFlow flow;
    address outputToken;
    uint256 keyIndex;
    DepositPayload encrypted;
    uint128 minVaultAssets;
    uint128 minVaultShares;
    uint128 minOutputAmount;
    bytes32 actionId;
    address tempoRefundRecipient;
}

/// @notice Callback-only stand-in until the production ZoneGateway is implemented.
/// @dev It owns its callback payload and models only the two synchronous
///      closed-loop returns. Token conversion/vault behavior is intentionally not modeled.
contract MockZoneGateway is IWithdrawalReceiver {

    error ApprovalFailed();
    error InsufficientOutputAmount(uint128 outputAmount, uint128 minOutputAmount);
    error InvalidOutputToken();
    error UnauthorizedMessenger();
    error UnregisteredGateway();
    error EncryptedPayloadAlreadyConsumed(bytes32 nullifier);

    /// @notice Encrypted deposit payloads already forwarded by this shared gateway.
    mapping(bytes32 nullifier => bool consumed) public payloads;

    bool public returnToZone = true;

    function setReturnToZone(bool enabled) external {
        returnToZone = enabled;
    }

    function onWithdrawalReceived(
        uint32,
        address sourcePortal,
        bytes32,
        address token,
        uint128 amount,
        bytes calldata callbackData
    )
        external
        returns (bytes4)
    {
        IZonePortal portal = IZonePortal(sourcePortal);
        if (msg.sender != portal.messenger()) revert UnauthorizedMessenger();
        if (portal.role(address(this)) != Role.CallbackGateway) {
            revert UnregisteredGateway();
        }

        (bytes32 callbackId, bytes memory gatewayData) = abi.decode(callbackData, (bytes32, bytes));
        GatewayCallbackData memory callback = abi.decode(gatewayData, (GatewayCallbackData));
        if (callback.outputToken != token) revert InvalidOutputToken();
        if (portal.role(callback.tempoRefundRecipient) != Role.Account) {
            revert IZonePortal.AccountNotAllowed(callback.tempoRefundRecipient);
        }
        if (amount < callback.minOutputAmount) {
            revert InsufficientOutputAmount(amount, callback.minOutputAmount);
        }

        if (!returnToZone) return IWithdrawalReceiver.onWithdrawalReceived.selector;

        bytes32 nullifier = EncryptedDepositLib.payloadNullifier(callback.encrypted);
        if (payloads[nullifier]) revert EncryptedPayloadAlreadyConsumed(nullifier);
        payloads[nullifier] = true;

        // The mock returns the received token. The production gateway owns conversion semantics.
        if (!ITIP20(token).approve(sourcePortal, amount)) revert ApprovalFailed();
        IZonePortal(sourcePortal)
            .depositWithContext(
                token,
                amount,
                callback.keyIndex,
                callback.encrypted,
                callback.tempoRefundRecipient,
                sourcePortal,
                callbackId
            );
        if (!ITIP20(token).approve(sourcePortal, 0)) revert ApprovalFailed();

        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }

}
