// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    IZoneFactory,
    ZONE_MESSENGER_ADDRESS,
    ZONE_VERIFIER_ADDRESS,
    ZoneInfo
} from "../../src/interfaces/IZone.sol";
import { ZoneFactory } from "../../src/tempo/ZoneFactory.sol";
import { ZonePortal } from "../../src/tempo/ZonePortal.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { Test } from "forge-std/Test.sol";
import { Vm } from "forge-std/Vm.sol";

contract ZoneFactoryAbiTest is Test {

    function test_createZone_selectorMatchesTip1091() public pure {
        assertEq(IZoneFactory.createZone.selector, bytes4(0xf2c58f2b));
    }

}

contract ZoneFactoryTest is BaseTest {

    ZoneFactory public zoneFactory;

    function setUp() public override {
        super.setUp();
        zoneFactory = _deployZoneFactory();
    }

    function _sequencers(address member) internal pure returns (address[] memory members) {
        members = new address[](1);
        members[0] = member;
    }

    function _defaultParams() internal view returns (IZoneFactory.CreateZoneParams memory) {
        return IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            admin: admin,
            sequencers: _sequencers(sequencer),
            threshold: 1,
            rpcUrl: ""
        });
    }

    function test_createZone_success() public {
        IZoneFactory.CreateZoneParams memory params = _defaultParams();
        params.rpcUrl = "https://zone.example";

        vm.recordLogs();
        (uint32 zoneId, address portal) = zoneFactory.createZone(params);

        assertEq(zoneId, 1);
        assertEq(zoneFactory.nextZoneId(), 2);
        assertTrue(zoneFactory.isZonePortal(portal));

        ZoneInfo memory info = zoneFactory.zones(zoneId);
        assertEq(info.zoneId, zoneId);
        assertEq(info.portal, portal);
        assertEq(info.admin, admin);
        assertEq(info.sequencers, params.sequencers);
        assertEq(info.threshold, 1);
        assertEq(info.verifier, ZONE_VERIFIER_ADDRESS);
        assertEq(info.rpcUrl, params.rpcUrl);

        ZonePortal created = ZonePortal(portal);
        assertEq(created.admin(), admin);
        assertEq(created.sequencerAt(0), sequencer);
        assertEq(created.sequencerThreshold(), 1);
        assertEq(created.verifier(), ZONE_VERIFIER_ADDRESS);
        assertEq(created.messenger(), ZONE_MESSENGER_ADDRESS);
        assertEq(created.blockHash(), bytes32(0));

        Vm.Log[] memory logs = vm.getRecordedLogs();
        bytes32 signature =
            keccak256("ZoneCreated(uint32,address,address,address,address[],uint8,address)");
        bool found;
        for (uint256 i = 0; i < logs.length; ++i) {
            if (logs[i].topics[0] == signature) found = true;
        }
        assertTrue(found, "ZoneCreated event not found");
    }

    function test_createZone_revertsForNonOwner() public {
        vm.prank(alice);
        vm.expectRevert(IZoneFactory.NotOwner.selector);
        zoneFactory.createZone(_defaultParams());
    }

    function test_createZone_revertsForInvalidInputs() public {
        IZoneFactory.CreateZoneParams memory params = _defaultParams();
        params.initialToken = address(0);
        vm.expectRevert(IZoneFactory.InvalidToken.selector);
        zoneFactory.createZone(params);

        params = _defaultParams();
        params.admin = address(0);
        vm.expectRevert(IZoneFactory.InvalidAdmin.selector);
        zoneFactory.createZone(params);

        params = _defaultParams();
        params.sequencers[0] = address(0);
        vm.expectRevert(IZoneFactory.InvalidSequencerSet.selector);
        zoneFactory.createZone(params);

        params = _defaultParams();
        params.threshold = 2;
        vm.expectRevert(IZoneFactory.InvalidSequencerSet.selector);
        zoneFactory.createZone(params);
    }

    function test_createZone_acceptsUnsortedUniqueSequencers() public {
        IZoneFactory.CreateZoneParams memory params = _defaultParams();
        params.sequencers = new address[](2);
        params.sequencers[0] = address(0x200);
        params.sequencers[1] = address(0x100);
        params.threshold = 2;

        (uint32 zoneId,) = zoneFactory.createZone(params);
        assertEq(zoneFactory.zones(zoneId).sequencers, params.sequencers);
    }

    function test_transferOwnership_allowsZeroAddress() public {
        vm.expectEmit(true, true, false, false);
        emit IZoneFactory.OwnershipTransferred(address(this), address(0));
        zoneFactory.transferOwnership(address(0));
        assertEq(zoneFactory.owner(), address(0));
    }

    function test_lockImplementationUpdates_isPermanentForReference() public {
        zoneFactory.lockImplementationUpdates();
        assertTrue(zoneFactory.implementationUpdatesLocked());

        vm.expectRevert(IZoneFactory.ImplementationUpdatesLocked.selector);
        zoneFactory.setPortalImplementation(address(this));
    }

    function test_zones_areCreationSnapshots() public {
        (uint32 id, address portal) = zoneFactory.createZone(_defaultParams());
        address[] memory replacement = _sequencers(alice);

        vm.prank(admin);
        ZonePortal(portal).setSequencerSet(replacement, 1);

        assertEq(ZonePortal(portal).sequencerAt(0), alice);
        assertEq(zoneFactory.zones(id).sequencers[0], sequencer);
    }

}
