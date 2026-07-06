// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IWithdrawalReceiver } from "../../src/interfaces/IZone.sol";
import { ZoneMessenger } from "../../src/tempo/ZoneMessenger.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { MockZoneToken } from "../mocks/MockZoneToken.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";

contract AcceptingWithdrawalReceiver is IWithdrawalReceiver {

    function onWithdrawalReceived(
        bytes32,
        address,
        uint128,
        bytes calldata
    )
        external
        pure
        returns (bytes4)
    {
        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }

}

contract RejectingWithdrawalReceiver is IWithdrawalReceiver {

    function onWithdrawalReceived(
        bytes32,
        address,
        uint128,
        bytes calldata
    )
        external
        pure
        returns (bytes4)
    {
        return bytes4(0xdeadbeef);
    }

}

contract ZoneMessengerTest is BaseTest {

    ZoneMessenger public messenger;
    MockZoneToken public zoneToken;
    address public portal = address(0x700);
    address public token = address(0x701);

    function setUp() public override {
        super.setUp();
        messenger = new ZoneMessenger(portal);
        zoneToken = new MockZoneToken("Zone USD", "zUSD");
        zoneToken.setMinter(address(this), true);
    }

    function _mockTransferFrom(address target, uint128 amount, bool result) internal {
        vm.mockCall(
            token,
            abi.encodeWithSelector(ITIP20.transferFrom.selector, portal, target, amount),
            abi.encode(result)
        );
    }

    /// @notice Verifies the messenger stores the portal address immutably.
    function test_portalImmutable() public view {
        assertEq(messenger.portal(), portal);
    }

    /// @notice Verifies only the portal can relay withdrawal messages.
    function test_relayMessage_revertsOnlyPortalForNonPortalCaller() public {
        vm.expectRevert(ZoneMessenger.OnlyPortal.selector);
        messenger.relayMessage(token, bytes32("sender"), alice, 1, 50_000, "");
    }

    /// @notice Verifies relay reverts when token transferFrom returns false.
    function test_relayMessage_revertsTransferFailedWhenTransferFromReturnsFalse() public {
        AcceptingWithdrawalReceiver receiver = new AcceptingWithdrawalReceiver();
        _mockTransferFrom(address(receiver), 1, false);

        vm.prank(portal);
        vm.expectRevert(ZoneMessenger.TransferFailed.selector);
        messenger.relayMessage(token, bytes32("sender"), address(receiver), 1, 50_000, "");
    }

    /// @notice Verifies relay reverts when the receiver returns the wrong selector.
    function test_relayMessage_revertsCallbackRejectedForWrongSelector() public {
        RejectingWithdrawalReceiver receiver = new RejectingWithdrawalReceiver();
        _mockTransferFrom(address(receiver), 1, true);

        vm.prank(portal);
        vm.expectRevert(ZoneMessenger.CallbackRejected.selector);
        messenger.relayMessage(token, bytes32("sender"), address(receiver), 1, 50_000, "");
    }

    /// @notice Verifies relay to an EOA target reverts after transfer succeeds.
    function test_relayMessage_revertsForEoaTarget() public {
        _mockTransferFrom(alice, 1, true);

        vm.prank(portal);
        vm.expectRevert();
        messenger.relayMessage(token, bytes32("sender"), alice, 1, 50_000, "");
    }

    /// @notice Verifies a valid relay transfers tokens to an accepting receiver.
    function test_relayMessage_success() public {
        AcceptingWithdrawalReceiver receiver = new AcceptingWithdrawalReceiver();
        bytes32 senderTag = keccak256("sender");
        bytes memory data = hex"1234";
        zoneToken.mint(portal, 123);
        vm.prank(portal);
        zoneToken.approve(address(messenger), 123);

        vm.prank(portal);
        messenger.relayMessage(
            address(zoneToken), senderTag, address(receiver), 123, 1_000_000, data
        );

        assertEq(zoneToken.balanceOf(address(receiver)), 123);
    }

    /// @notice Verifies valid relays transfer any bounded amount to the receiver.
    function testFuzz_relayMessage_success(
        uint128 amount,
        uint64 gasLimit,
        bytes calldata data
    )
        public
    {
        vm.assume(gasLimit >= 500_000);
        amount = uint128(bound(amount, 0, 1_000_000_000e6));
        AcceptingWithdrawalReceiver receiver = new AcceptingWithdrawalReceiver();
        bytes32 senderTag = keccak256(abi.encode(amount, gasLimit, data));
        zoneToken.mint(portal, amount);
        vm.prank(portal);
        zoneToken.approve(address(messenger), amount);

        vm.prank(portal);
        messenger.relayMessage(
            address(zoneToken), senderTag, address(receiver), amount, gasLimit, data
        );

        assertEq(zoneToken.balanceOf(address(receiver)), amount);
    }

}
