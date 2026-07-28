// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    IWithdrawalReceiver,
    IZoneFactory,
    IZoneMessenger,
    IZonePortal,
    Role,
    ZONE_FACTORY_ADDRESS,
    ZoneInfo
} from "../interfaces/IZone.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";

/// @title ZoneMessenger
/// @notice Shared withdrawal callback sender for all zones created by one ZoneFactory.
contract ZoneMessenger is IZoneMessenger {

    IZoneFactory public constant zoneFactory = IZoneFactory(ZONE_FACTORY_ADDRESS);

    uint256 internal _relayReentrancyStatus;

    error UnauthorizedPortal();
    error TransferFailed();
    error CallbackRejected();
    error InvalidCallbackTarget();
    error ReentrantRelay();

    modifier nonReentrantRelay() {
        if (_relayReentrancyStatus != 0) revert ReentrantRelay();
        _relayReentrancyStatus = 1;
        _;
        _relayReentrancyStatus = 0;
    }

    function relayMessage(
        uint32 zoneId,
        address token,
        bytes32 senderTag,
        address target,
        uint128 amount,
        uint64 gasLimit,
        bytes calldata data
    )
        external
        nonReentrantRelay
    {
        ZoneInfo memory zone = zoneFactory.zones(zoneId);
        if (zone.portal != msg.sender) revert UnauthorizedPortal();

        if (
            !IZonePortal(msg.sender).isGatewayOpen()
                && IZonePortal(msg.sender).role(target) != Role.CallbackGateway
        ) {
            revert InvalidCallbackTarget();
        }

        // Unlike the callback below, this call is deliberately left raw: `token` is not attacker
        // chosen. `enableToken` is admin-only and requires `isTIP20`, which restricts tokens to
        // the factory's reserved prefix, so `transfer` is precompile-backed and reverts with
        // constant-size errors. A token with arbitrary code could bubble an oversized revert blob
        // here — but it would also burn the uncapped gas this call forwards, so bounding the copy
        // would not save the batch. The boundary is the native-TIP-20 restriction, not this frame.
        // Invariant: TEMPO-ZONE-WITHDRAWAL-CALLBACK-RETURNDATA-BOUND.
        if (!ITIP20(token).transfer(target, amount)) {
            revert TransferFailed();
        }

        // A callback target is untrusted, so never copy its revert data. Propagating it charges
        // quadratic memory gas here and again in the portal's delivery frame, letting one
        // withdrawal burn many times its `gasLimit`. The parameterless `catch` discards it.
        // Invariant: TEMPO-ZONE-WITHDRAWAL-CALLBACK-RETURNDATA-BOUND.
        try IWithdrawalReceiver(target).onWithdrawalReceived{ gas: gasLimit }(
            zoneId, msg.sender, senderTag, token, amount, data
        ) returns (
            bytes4 selector
        ) {
            if (selector != IWithdrawalReceiver.onWithdrawalReceived.selector) {
                revert CallbackRejected();
            }
        } catch {
            revert CallbackRejected();
        }
    }

}
