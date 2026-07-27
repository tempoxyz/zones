// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    IWithdrawalReceiver,
    IZonePortal,
    MAX_WITHDRAWAL_CALLBACK_GAS,
    Role,
    ZONE_FACTORY_ADDRESS,
    ZONE_MESSENGER_ADDRESS,
    ZoneInfo
} from "../../src/interfaces/IZone.sol";
import { ZoneMessenger } from "../../src/tempo/ZoneMessenger.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { MockZoneToken } from "../mocks/MockZoneToken.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";

contract MockZoneFactoryForMessenger {

    mapping(uint32 => ZoneInfo) internal _zones;

    function setPortal(uint32 zoneId, address portal) external {
        _zones[zoneId].zoneId = zoneId;
        _zones[zoneId].portal = portal;
    }

    function zones(uint32 id) external view returns (ZoneInfo memory) {
        return _zones[id];
    }

}

contract AcceptingWithdrawalReceiver is IWithdrawalReceiver {

    uint32 public lastZoneId;
    address public lastSourcePortal;
    bytes32 public lastSenderTag;
    address public lastToken;
    uint128 public lastAmount;
    bytes public lastData;

    function onWithdrawalReceived(
        uint32 zoneId,
        address sourcePortal,
        bytes32 senderTag,
        address token,
        uint128 amount,
        bytes calldata callbackData
    )
        external
        returns (bytes4)
    {
        lastZoneId = zoneId;
        lastSourcePortal = sourcePortal;
        lastSenderTag = senderTag;
        lastToken = token;
        lastAmount = amount;
        lastData = callbackData;
        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }

}

contract RejectingWithdrawalReceiver is IWithdrawalReceiver {

    function onWithdrawalReceived(
        uint32,
        address,
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

/// @dev Reverts with `bombSize` bytes of revert data. Producing the blob only costs this
/// frame's own memory expansion, out of the gas the messenger forwarded; the damage is that a
/// caller which propagates the revert must `returndatacopy` the whole blob into its own frame.
contract RevertBombReceiver is IWithdrawalReceiver {

    uint256 public bombSize;

    constructor(uint256 size) {
        bombSize = size;
    }

    function onWithdrawalReceived(
        uint32,
        address,
        bytes32,
        address,
        uint128,
        bytes calldata
    )
        external
        view
        returns (bytes4)
    {
        uint256 size = bombSize;
        assembly {
            revert(0, size)
        }
    }

}

/// @dev Returns the expected selector followed by `bombSize` bytes of padding.
contract ReturnBombReceiver is IWithdrawalReceiver {

    uint256 public bombSize;

    constructor(uint256 size) {
        bombSize = size;
    }

    function onWithdrawalReceived(
        uint32,
        address,
        bytes32,
        address,
        uint128,
        bytes calldata
    )
        external
        view
        returns (bytes4)
    {
        uint256 size = bombSize;
        bytes4 accepted = IWithdrawalReceiver.onWithdrawalReceived.selector;
        assembly {
            mstore(0, accepted)
            return(0, size)
        }
    }

}

/// @dev Returns exactly one word whose leading selector is correct but whose low-order padding
/// is dirty. Solidity's `bytes4` decoder must keep rejecting this.
contract DirtyPaddingReceiver is IWithdrawalReceiver {

    function onWithdrawalReceived(
        uint32,
        address,
        bytes32,
        address,
        uint128,
        bytes calldata
    )
        external
        pure
        returns (bytes4)
    {
        bytes4 accepted = IWithdrawalReceiver.onWithdrawalReceived.selector;
        assembly {
            mstore(0, or(accepted, not(shl(224, 0xffffffff))))
            return(0, 32)
        }
    }

}

/// @dev Returns a well-formed selector word followed by a second, ignorable word.
contract OverlongReceiver is IWithdrawalReceiver {

    function onWithdrawalReceived(
        uint32,
        address,
        bytes32,
        address,
        uint128,
        bytes calldata
    )
        external
        pure
        returns (bytes4)
    {
        bytes4 accepted = IWithdrawalReceiver.onWithdrawalReceived.selector;
        assembly {
            mstore(0, accepted)
            mstore(32, not(0))
            return(0, 64)
        }
    }

}

/// @dev Returns fewer than 32 bytes, which cannot decode to a `bytes4`.
contract ShortReturnReceiver is IWithdrawalReceiver {

    function onWithdrawalReceived(
        uint32,
        address,
        bytes32,
        address,
        uint128,
        bytes calldata
    )
        external
        pure
        returns (bytes4)
    {
        assembly {
            return(0, 4)
        }
    }

}

contract ZoneMessengerTest is BaseTest {

    uint32 internal constant ZONE_ID = 1;
    uint32 internal constant OTHER_ZONE_ID = 2;
    uint64 internal constant CALLBACK_GAS_LIMIT = MAX_WITHDRAWAL_CALLBACK_GAS;

    MockZoneFactoryForMessenger public messengerFactory;
    ZoneMessenger public messenger;
    MockZoneToken public zoneToken;

    address public portal = address(0x700);
    address public otherPortal = address(0x702);
    address public token = address(0x701);

    function setUp() public override {
        super.setUp();
        vm.etch(ZONE_FACTORY_ADDRESS, type(MockZoneFactoryForMessenger).runtimeCode);
        messengerFactory = MockZoneFactoryForMessenger(ZONE_FACTORY_ADDRESS);
        messengerFactory.setPortal(ZONE_ID, portal);
        messengerFactory.setPortal(OTHER_ZONE_ID, otherPortal);

        vm.etch(ZONE_MESSENGER_ADDRESS, type(ZoneMessenger).runtimeCode);
        messenger = ZoneMessenger(ZONE_MESSENGER_ADDRESS);
        vm.mockCall(
            portal, abi.encodeWithSelector(IZonePortal.isGatewayOpen.selector), abi.encode(false)
        );
        zoneToken = new MockZoneToken("Zone USD", "zUSD");
        zoneToken.setMinter(address(this), true);
    }

    function _mockTransfer(address target, uint128 amount, bool result) internal {
        _allowGateway(target);
        vm.mockCall(
            token,
            abi.encodeWithSelector(ITIP20.transfer.selector, target, amount),
            abi.encode(result)
        );
    }

    function _allowGateway(address target) internal {
        vm.mockCall(
            portal,
            abi.encodeWithSelector(IZonePortal.role.selector, target),
            abi.encode(Role.CallbackGateway)
        );
    }

    function _callback() internal pure returns (bytes memory) {
        return hex"010203";
    }

    function test_zoneFactoryConstant() public view {
        assertEq(address(messenger.zoneFactory()), address(messengerFactory));
    }

    function test_relayMessage_revertsUnauthorizedPortalForNonPortalCaller() public {
        vm.expectRevert(ZoneMessenger.UnauthorizedPortal.selector);
        messenger.relayMessage(ZONE_ID, token, bytes32("sender"), alice, 1, 50_000, "");
    }

    function test_relayMessage_revertsUnauthorizedPortalForWrongZoneId() public {
        vm.prank(portal);
        vm.expectRevert(ZoneMessenger.UnauthorizedPortal.selector);
        messenger.relayMessage(OTHER_ZONE_ID, token, bytes32("sender"), alice, 1, 50_000, "");
    }

    function test_relayMessage_revertsTransferFailedWhenTransferReturnsFalse() public {
        AcceptingWithdrawalReceiver receiver = new AcceptingWithdrawalReceiver();
        _mockTransfer(address(receiver), 1, false);

        vm.prank(portal);
        vm.expectRevert(ZoneMessenger.TransferFailed.selector);
        messenger.relayMessage(
            ZONE_ID, token, bytes32("sender"), address(receiver), 1, 50_000, _callback()
        );
    }

    function test_relayMessage_revertsCallbackRejectedForWrongSelector() public {
        RejectingWithdrawalReceiver receiver = new RejectingWithdrawalReceiver();
        _mockTransfer(address(receiver), 1, true);

        vm.prank(portal);
        vm.expectRevert(ZoneMessenger.CallbackRejected.selector);
        messenger.relayMessage(
            ZONE_ID, token, bytes32("sender"), address(receiver), 1, 50_000, _callback()
        );
    }

    function test_relayMessage_revertsForEoaTarget() public {
        _mockTransfer(alice, 1, true);

        vm.prank(portal);
        vm.expectRevert();
        messenger.relayMessage(ZONE_ID, token, bytes32("sender"), alice, 1, 50_000, _callback());
    }

    function test_relayMessage_successWithFlattenedFactoryGetter() public {
        AcceptingWithdrawalReceiver receiver = new AcceptingWithdrawalReceiver();
        bytes32 senderTag = keccak256("sender");
        bytes memory data = _callback();
        zoneToken.mint(address(messenger), 123);
        _allowGateway(address(receiver));

        vm.prank(portal);
        messenger.relayMessage(
            ZONE_ID, address(zoneToken), senderTag, address(receiver), 123, CALLBACK_GAS_LIMIT, data
        );

        assertEq(zoneToken.balanceOf(address(receiver)), 123);
        assertEq(receiver.lastZoneId(), ZONE_ID);
        assertEq(receiver.lastSourcePortal(), portal);
        assertEq(receiver.lastSenderTag(), senderTag);
        assertEq(receiver.lastToken(), address(zoneToken));
        assertEq(receiver.lastAmount(), 123);
        assertEq(receiver.lastData(), data);
    }

    function testFuzz_relayMessage_success(uint128 amount, bool redeem) public {
        amount = uint128(bound(amount, 0, 1_000_000_000e6));
        bytes memory data = abi.encode(redeem);
        AcceptingWithdrawalReceiver receiver = new AcceptingWithdrawalReceiver();
        bytes32 senderTag = keccak256(abi.encode(amount, data));
        zoneToken.mint(address(messenger), amount);
        _allowGateway(address(receiver));

        vm.prank(portal);
        messenger.relayMessage(
            ZONE_ID,
            address(zoneToken),
            senderTag,
            address(receiver),
            amount,
            CALLBACK_GAS_LIMIT,
            data
        );

        assertEq(zoneToken.balanceOf(address(receiver)), amount);
    }

    function test_relayMessage_forwardsOpaqueData() public {
        AcceptingWithdrawalReceiver receiver = new AcceptingWithdrawalReceiver();
        bytes memory data = abi.encode(uint256(2));
        zoneToken.mint(address(messenger), 1);
        _allowGateway(address(receiver));

        vm.prank(portal);
        messenger.relayMessage(
            ZONE_ID,
            address(zoneToken),
            bytes32("sender"),
            address(receiver),
            1,
            CALLBACK_GAS_LIMIT,
            data
        );

        assertEq(zoneToken.balanceOf(address(receiver)), 1);
        assertEq(receiver.lastData(), data);
    }

    function test_relayMessage_revertsForUnregisteredGateway() public {
        AcceptingWithdrawalReceiver receiver = new AcceptingWithdrawalReceiver();
        vm.mockCall(
            portal,
            abi.encodeWithSelector(IZonePortal.role.selector, address(receiver)),
            abi.encode(Role.None)
        );

        vm.prank(portal);
        vm.expectRevert(ZoneMessenger.InvalidCallbackTarget.selector);
        messenger.relayMessage(
            ZONE_ID,
            address(zoneToken),
            bytes32("sender"),
            address(receiver),
            1,
            50_000,
            _callback()
        );
    }

    /*//////////////////////////////////////////////////////////////
                        UNTRUSTED RETURN DATA BOUNDS
    //////////////////////////////////////////////////////////////*/

    /// @dev Relays to `target` and reports the gas charged to the messenger's caller frame.
    function _relayCost(address target) internal returns (uint256 burned) {
        zoneToken.mint(address(messenger), 1);
        _allowGateway(target);

        vm.prank(portal);
        uint256 before = gasleft();
        try messenger.relayMessage(
            ZONE_ID, address(zoneToken), bytes32("sender"), target, 1, CALLBACK_GAS_LIMIT, ""
        ) { }
            catch { }
        burned = before - gasleft();
    }

    /// A callback that reverts with megabytes must not charge the copy to the messenger's frame.
    /// Without a bounded `catch`, solc's revert-forwarder copies the blob at quadratic cost, so a
    /// single withdrawal can burn several times the `gasLimit` it declared and paid for, starving
    /// the remaining items in a `processWithdrawals` batch.
    function test_relayMessage_revertBombDoesNotAmplifyCallerGas() public {
        uint256 honest = _relayCost(address(new RevertBombReceiver(4)));
        uint256 bombed = _relayCost(address(new RevertBombReceiver(900_000)));

        // The callee still pays its own memory expansion out of the forwarded `gasLimit`; what
        // must not happen is the messenger paying to copy the blob a second time.
        uint256 calleeExpansion = _memoryCost(900_000);
        assertLt(bombed, honest + calleeExpansion + 30_000);
    }

    /// The same bound must hold for oversized success returns. This one already held before the
    /// bounded `catch`, because a static `bytes4` return is decoded from a fixed 32-byte window
    /// rather than from `returndatasize()`; it is here to keep that property from regressing if
    /// the return type ever becomes dynamic.
    function test_relayMessage_returnBombDoesNotAmplifyCallerGas() public {
        uint256 honest = _relayCost(address(new ReturnBombReceiver(32)));
        uint256 bombed = _relayCost(address(new ReturnBombReceiver(900_000)));

        uint256 calleeExpansion = _memoryCost(900_000);
        assertLt(bombed, honest + calleeExpansion + 30_000);
    }

    /// Cost of expanding a fresh frame's memory to `size` bytes, per the EVM's quadratic formula.
    function _memoryCost(uint256 size) internal pure returns (uint256) {
        uint256 words = (size + 31) / 32;
        return 3 * words + (words * words) / 512;
    }

    /// Bounding the copy must not change which responses are accepted. These shapes pin the
    /// decoder's behaviour so a later refactor cannot quietly loosen it.
    ///
    /// A `catch` clause deliberately does not catch failures in decoding the callee's return
    /// data, so a malformed response still reverts with empty data rather than
    /// `CallbackRejected` — unchanged from before this bound was introduced. Either way the
    /// portal's `try this.deliverWithdrawal(...)` catches the failure and bounces the withdrawal.
    function test_relayMessage_acceptsOnlyWellFormedSelectorWord() public {
        zoneToken.mint(address(messenger), 4);

        address overlong = address(new OverlongReceiver());
        _allowGateway(overlong);
        vm.prank(portal);
        messenger.relayMessage(
            ZONE_ID, address(zoneToken), bytes32("sender"), overlong, 1, CALLBACK_GAS_LIMIT, ""
        );

        address dirty = address(new DirtyPaddingReceiver());
        _allowGateway(dirty);
        vm.prank(portal);
        vm.expectRevert();
        messenger.relayMessage(
            ZONE_ID, address(zoneToken), bytes32("sender"), dirty, 1, CALLBACK_GAS_LIMIT, ""
        );

        address short = address(new ShortReturnReceiver());
        _allowGateway(short);
        vm.prank(portal);
        vm.expectRevert();
        messenger.relayMessage(
            ZONE_ID, address(zoneToken), bytes32("sender"), short, 1, CALLBACK_GAS_LIMIT, ""
        );
    }

    /// A reverting callback surfaces as `CallbackRejected`, never as the callee's own revert data.
    function test_relayMessage_revertingCallbackSurfacesAsCallbackRejected() public {
        address bomb = address(new RevertBombReceiver(64));
        zoneToken.mint(address(messenger), 1);
        _allowGateway(bomb);

        vm.prank(portal);
        vm.expectRevert(ZoneMessenger.CallbackRejected.selector);
        messenger.relayMessage(
            ZONE_ID, address(zoneToken), bytes32("sender"), bomb, 1, CALLBACK_GAS_LIMIT, ""
        );
    }

}
