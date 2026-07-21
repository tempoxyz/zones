// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

interface IZoneWithdrawalReceiver {

    function onWithdrawalReceived(
        uint32 zoneId,
        address sourcePortal,
        bytes32 senderTag,
        address token,
        uint128 amount,
        bytes calldata callbackData
    )
        external
        returns (bytes4);

}
