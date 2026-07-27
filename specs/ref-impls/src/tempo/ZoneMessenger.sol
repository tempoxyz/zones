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

        if (!ITIP20(token).transfer(target, amount)) {
            revert TransferFailed();
        }

        // A callback target is untrusted, so its revert data must never be copied into this
        // frame. Letting the revert propagate makes solc's revert-forwarder run
        // `returndatacopy(pos, 0, returndatasize())`, so a target that reverts with megabytes
        // charges quadratic memory-expansion gas here and again in the portal's delivery frame.
        // That lets one withdrawal burn several times its own `gasLimit` and starve the rest of
        // the `processWithdrawals` batch. A parameterless `catch` discards the revert data, which
        // keeps delivery within `gasLimit` plus this contract's fixed overhead.
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
