// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { CallbackData, IWithdrawalReceiver, IZonePortal } from "../../src/interfaces/IZone.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";

/// @notice Callback-only stand-in until the production ZoneGateway is implemented.
/// @dev It decodes the canonical callback payload and models only the two synchronous
///      closed-loop returns. Token conversion/vault behavior is intentionally not modeled.
contract MockZoneGateway is IWithdrawalReceiver {

    error ApprovalFailed();
    error InsufficientOutputAmount(uint128 outputAmount, uint128 minOutputAmount);
    error InvalidOutputToken();
    error UnauthorizedMessenger();
    error UnregisteredGateway();

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
        if (!portal.zoneGateway(address(this))) revert UnregisteredGateway();

        CallbackData memory callback = abi.decode(callbackData, (CallbackData));
        if (callback.outputToken != token) revert InvalidOutputToken();
        if (!portal.allowedAccount(callback.tempoRefundRecipient)) {
            revert IZonePortal.AccountNotAllowed(callback.tempoRefundRecipient);
        }
        if (amount < callback.minOutputAmount) {
            revert InsufficientOutputAmount(amount, callback.minOutputAmount);
        }

        if (!returnToZone) return IWithdrawalReceiver.onWithdrawalReceived.selector;

        // The mock returns the received token. The production gateway owns conversion semantics.
        if (!ITIP20(token).approve(sourcePortal, amount)) revert ApprovalFailed();
        IZonePortal(sourcePortal)
            .depositEncrypted(
                token, amount, callback.keyIndex, callback.encrypted, callback.tempoRefundRecipient
            );
        if (!ITIP20(token).approve(sourcePortal, 0)) revert ApprovalFailed();

        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }

}
