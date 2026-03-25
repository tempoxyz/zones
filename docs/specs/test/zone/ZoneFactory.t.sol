// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { TIP20 } from "../../src/TIP20.sol";
import { IZoneFactory, ZoneInfo, ZoneParams } from "../../src/zone/IZone.sol";
import { ZoneFactory } from "../../src/zone/ZoneFactory.sol";
import { ZoneMessenger } from "../../src/zone/ZoneMessenger.sol";
import { ZonePortal } from "../../src/zone/ZonePortal.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { Vm } from "forge-std/Vm.sol";

/// @title ZoneFactoryTest
/// @notice Comprehensive tests for ZoneFactory validation and zone creation
contract ZoneFactoryTest is BaseTest {

    ZoneFactory public zoneFactory;

    bytes32 constant GENESIS_BLOCK_HASH = keccak256("genesis");
    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");

    function setUp() public override {
        super.setUp();
        zoneFactory = new ZoneFactory();
    }

    /*//////////////////////////////////////////////////////////////
                          VALID CREATION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_createZone_success() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            sequencer: admin,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            })
        });

        (uint32 zoneId, address portal) = zoneFactory.createZone(params);

        assertEq(zoneId, 1);
        assertTrue(portal != address(0));
        assertEq(zoneFactory.zoneCount(), 1);
        assertTrue(zoneFactory.isZonePortal(portal));

        ZoneInfo memory info = zoneFactory.zones(zoneId);
        assertEq(info.zoneId, 1);
        assertEq(info.portal, portal);
        assertTrue(info.messenger != address(0));
        assertEq(info.initialToken, address(pathUSD));
        assertEq(info.sequencer, admin);
        assertEq(info.verifier, zoneFactory.verifier());
        assertEq(info.genesisBlockHash, GENESIS_BLOCK_HASH);
        assertEq(info.genesisTempoBlockHash, GENESIS_TEMPO_BLOCK_HASH);
    }

    function test_createZone_deploysMessenger() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            sequencer: admin,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            })
        });

        (uint32 zoneId, address portal) = zoneFactory.createZone(params);

        ZoneInfo memory info = zoneFactory.zones(zoneId);
        address messengerAddr = info.messenger;

        // Verify messenger is deployed and configured correctly
        ZoneMessenger messenger = ZoneMessenger(messengerAddr);
        assertEq(messenger.portal(), portal);

        // Verify portal references the messenger
        ZonePortal portalContract = ZonePortal(portal);
        assertEq(portalContract.messenger(), messengerAddr);
    }

    function test_createZone_multipleZones() public {
        IZoneFactory.CreateZoneParams memory params1 = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            sequencer: admin,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            })
        });

        (uint32 zoneId1, address portal1) = zoneFactory.createZone(params1);

        IZoneFactory.CreateZoneParams memory params2 = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            sequencer: alice,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: keccak256("genesis2"),
                genesisTempoBlockHash: keccak256("tempoGenesis2"),
                genesisTempoBlockNumber: uint64(block.number)
            })
        });

        (uint32 zoneId2, address portal2) = zoneFactory.createZone(params2);

        assertEq(zoneId1, 1);
        assertEq(zoneId2, 2);
        assertTrue(portal1 != portal2);
        assertEq(zoneFactory.zoneCount(), 2);
        assertTrue(zoneFactory.isZonePortal(portal1));
        assertTrue(zoneFactory.isZonePortal(portal2));

        // Each zone should have its own messenger
        ZoneInfo memory info1 = zoneFactory.zones(zoneId1);
        ZoneInfo memory info2 = zoneFactory.zones(zoneId2);
        assertTrue(info1.messenger != info2.messenger);
    }

    function test_createZone_emitsEvent() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            sequencer: admin,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            })
        });

        // Record logs and verify ZoneCreated event was emitted
        vm.recordLogs();
        (uint32 zoneId, address portal) = zoneFactory.createZone(params);

        // Verify logs contain ZoneCreated event with correct data
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bool found = false;
        for (uint256 i = 0; i < logs.length; i++) {
            if (
                logs[i].topics[0]
                    == keccak256(
                        "ZoneCreated(uint32,address,address,address,address,address,bytes32,bytes32,uint64)"
                    )
            ) {
                found = true;
                // Verify the indexed zoneId (topic[1])
                assertEq(uint256(logs[i].topics[1]), uint256(zoneId));
                // Verify indexed portal (topic[2])
                assertEq(address(uint160(uint256(logs[i].topics[2]))), portal);
                break;
            }
        }
        assertTrue(found, "ZoneCreated event not found");

        // Verify the portal address is valid
        assertTrue(portal != address(0));
    }

    /*//////////////////////////////////////////////////////////////
                          INVALID TOKEN TESTS
    //////////////////////////////////////////////////////////////*/

    function test_createZone_revertsOnInvalidToken_zeroAddress() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: address(0),
            sequencer: admin,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            })
        });

        vm.expectRevert(IZoneFactory.InvalidToken.selector);
        zoneFactory.createZone(params);
    }

    function test_createZone_revertsOnInvalidToken_nonTIP20() public {
        // Deploy a non-TIP20 contract (just an empty contract)
        address notTip20 = address(new NotATIP20());

        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: notTip20,
            sequencer: admin,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            })
        });

        vm.expectRevert(IZoneFactory.InvalidToken.selector);
        zoneFactory.createZone(params);
    }

    function test_createZone_revertsOnInvalidToken_eoa() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: alice, // EOA, not a contract
            sequencer: admin,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            })
        });

        vm.expectRevert(IZoneFactory.InvalidToken.selector);
        zoneFactory.createZone(params);
    }

    /*//////////////////////////////////////////////////////////////
                       INVALID SEQUENCER TESTS
    //////////////////////////////////////////////////////////////*/

    function test_createZone_revertsOnInvalidSequencer_zeroAddress() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            sequencer: address(0),
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            })
        });

        vm.expectRevert(IZoneFactory.InvalidSequencer.selector);
        zoneFactory.createZone(params);
    }

    /*//////////////////////////////////////////////////////////////
                       INVALID VERIFIER TESTS
    //////////////////////////////////////////////////////////////*/

    function test_createZone_revertsOnInvalidVerifier() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            sequencer: admin,
            verifier: address(0xdead),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            })
        });

        vm.expectRevert(IZoneFactory.InvalidVerifier.selector);
        zoneFactory.createZone(params);
    }

    /*//////////////////////////////////////////////////////////////
                            VIEW TESTS
    //////////////////////////////////////////////////////////////*/

    function test_zoneCount_initiallyZero() public view {
        assertEq(zoneFactory.zoneCount(), 0);
    }

    function test_isZonePortal_returnsFalseForNonPortal() public view {
        assertFalse(zoneFactory.isZonePortal(address(0)));
        assertFalse(zoneFactory.isZonePortal(alice));
        assertFalse(zoneFactory.isZonePortal(address(zoneFactory)));
    }

    function test_zones_returnsEmptyForNonExistentZone() public view {
        ZoneInfo memory info = zoneFactory.zones(999);
        assertEq(info.zoneId, 0);
        assertEq(info.portal, address(0));
        assertEq(info.messenger, address(0));
        assertEq(info.initialToken, address(0));
    }

}

/// @notice A minimal contract that is NOT a TIP-20
contract NotATIP20 {

    function notATIP20Function() external pure returns (bool) {
        return true;
    }

}
