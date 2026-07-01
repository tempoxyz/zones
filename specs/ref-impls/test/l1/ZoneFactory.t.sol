// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZoneFactory, ZoneInfo, ZoneParams } from "../../src/interfaces/IZone.sol";
import { ZoneFactory } from "../../src/l1/ZoneFactory.sol";
import { ZoneMessenger } from "../../src/l1/ZoneMessenger.sol";
import { ZonePortal } from "../../src/l1/ZonePortal.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { Vm } from "forge-std/Vm.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";

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
            admin: admin,
            sequencer: sequencer,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
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
        assertEq(info.admin, admin);
        assertEq(info.sequencer, sequencer);
        assertEq(info.verifier, zoneFactory.verifier());
        assertEq(info.genesisBlockHash, GENESIS_BLOCK_HASH);
        assertEq(info.genesisTempoBlockHash, GENESIS_TEMPO_BLOCK_HASH);
    }

    function test_createZone_deploysMessenger() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            admin: admin,
            sequencer: sequencer,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
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
            admin: admin,
            sequencer: sequencer,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
        });

        (uint32 zoneId1, address portal1) = zoneFactory.createZone(params1);

        address secondSequencer = alice;
        IZoneFactory.CreateZoneParams memory params2 = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            admin: admin,
            sequencer: secondSequencer,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: keccak256("genesis2"),
                genesisTempoBlockHash: keccak256("tempoGenesis2"),
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
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
        assertEq(info1.sequencer, sequencer);
        assertEq(info2.sequencer, secondSequencer);
        assertTrue(info1.messenger != info2.messenger);
    }

    function test_createZone_emitsEvent() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            admin: admin,
            sequencer: sequencer,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
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
                        "ZoneCreated(uint32,address,address,address,address,address,address,bytes32,bytes32,uint64)"
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
            admin: admin,
            sequencer: sequencer,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
        });

        vm.expectRevert(IZoneFactory.InvalidToken.selector);
        zoneFactory.createZone(params);
    }

    function test_createZone_revertsOnInvalidToken_nonTIP20() public {
        // Deploy a non-TIP20 contract (just an empty contract)
        address notTip20 = address(new NotATIP20());

        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: notTip20,
            admin: admin,
            sequencer: sequencer,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
        });

        vm.expectRevert(IZoneFactory.InvalidToken.selector);
        zoneFactory.createZone(params);
    }

    function test_createZone_revertsOnInvalidToken_eoa() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: alice, // EOA, not a contract
            admin: admin,
            sequencer: sequencer,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
        });

        vm.expectRevert(IZoneFactory.InvalidToken.selector);
        zoneFactory.createZone(params);
    }

    /*//////////////////////////////////////////////////////////////
                          INVALID ADMIN TESTS
    //////////////////////////////////////////////////////////////*/

    function test_createZone_revertsOnInvalidAdmin_zeroAddress() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            admin: address(0),
            sequencer: sequencer,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
        });

        vm.expectRevert(IZoneFactory.InvalidAdmin.selector);
        zoneFactory.createZone(params);
    }

    /*//////////////////////////////////////////////////////////////
                       INVALID SEQUENCER TESTS
    //////////////////////////////////////////////////////////////*/

    function test_createZone_revertsOnInvalidSequencer_zeroAddress() public {
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            admin: admin,
            sequencer: address(0),
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
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
            admin: admin,
            sequencer: sequencer,
            verifier: address(0xdead),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
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

    /*//////////////////////////////////////////////////////////////
                            SHARED HELPER
    //////////////////////////////////////////////////////////////*/

    function _defaultParams() internal view returns (IZoneFactory.CreateZoneParams memory) {
        return IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            admin: admin,
            sequencer: sequencer,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: uint64(block.number)
            }),
            rpcUrl: ""
        });
    }

    /*//////////////////////////////////////////////////////////////
                              REVERT PATHS
    //////////////////////////////////////////////////////////////*/

    // Supply < ZONE_CREATION_GAS (15M) so the gasleft() check trips. 14M is high
    // enough to reach the check (no OOG in the isTIP20 staticcall) and low enough
    // that gasleft() < 15M at the check.
    function test_createZone_revertsOnInsufficientGas() public {
        IZoneFactory.CreateZoneParams memory p = _defaultParams();
        vm.expectRevert(IZoneFactory.InsufficientGas.selector);
        zoneFactory.createZone{ gas: 14_000_000 }(p);
    }

    // _nextZoneId is storage slot 0 (uint32, packed alone).
    function test_createZone_revertsOnZoneIdOverflow() public {
        IZoneFactory.CreateZoneParams memory p = _defaultParams();
        vm.store(address(zoneFactory), bytes32(uint256(0)), bytes32(uint256(type(uint32).max)));
        vm.expectRevert(IZoneFactory.ZoneIdOverflow.selector);
        zoneFactory.createZone(p);
    }

    /*//////////////////////////////////////////////////////////////
                       PORTAL PARAM PROPAGATION
    //////////////////////////////////////////////////////////////*/

    function test_createZone_propagatesAllParamsToPortal() public {
        IZoneFactory.CreateZoneParams memory p = _defaultParams();
        p.rpcUrl = "https://zone.example";
        (uint32 id, address portal) = zoneFactory.createZone(p);
        ZonePortal pc = ZonePortal(portal);

        assertEq(pc.zoneId(), id);
        assertEq(pc.admin(), p.admin);
        assertEq(pc.sequencer(), p.sequencer);
        assertEq(pc.verifier(), p.verifier);
        assertEq(pc.messenger(), zoneFactory.zones(id).messenger);
        assertEq(pc.blockHash(), p.zoneParams.genesisBlockHash);
        assertEq(pc.genesisTempoBlockNumber(), p.zoneParams.genesisTempoBlockNumber);
        assertEq(pc.rpcUrl(), p.rpcUrl);
        assertTrue(pc.isTokenEnabled(address(pathUSD)));
        assertEq(pathUSD.allowance(portal, pc.messenger()), type(uint256).max);
    }

    function test_createZone_registersMessenger() public {
        (uint32 id,) = zoneFactory.createZone(_defaultParams());
        assertTrue(zoneFactory.isZoneMessenger(zoneFactory.zones(id).messenger));
        assertFalse(zoneFactory.isZoneMessenger(alice));
    }

    /*//////////////////////////////////////////////////////////////
                          METADATA ROTATION
    //////////////////////////////////////////////////////////////*/

    // Factory ZoneInfo.sequencer is a snapshot taken at creation and does not track
    // later portal rotation; the portal remains the source of truth.
    function test_zones_sequencerIsSnapshot_afterRotation() public {
        (uint32 id, address portal) = zoneFactory.createZone(_defaultParams());
        address nextSequencer = alice;

        vm.prank(sequencer);
        ZonePortal(portal).transferSequencer(nextSequencer);
        vm.prank(nextSequencer);
        ZonePortal(portal).acceptSequencer();

        assertEq(ZonePortal(portal).sequencer(), nextSequencer); // portal: current
        assertEq(zoneFactory.zones(id).sequencer, sequencer); // factory: snapshot at creation
    }

    /*//////////////////////////////////////////////////////////////
                    CREATE-ADDRESS RLP BOUNDARIES
    //////////////////////////////////////////////////////////////*/

    function test_computeCreateAddress_matchesReference_atRlpBoundaries() public {
        ZoneFactoryHarness h = new ZoneFactoryHarness();
        uint256[10] memory nonces =
            [uint256(0), 1, 0x7f, 0x80, 0xff, 0x100, 0xffff, 0x10000, 0xffffff, 0x1000000];
        for (uint256 i; i < nonces.length; i++) {
            assertEq(
                h.computeCreateAddress_(address(h), nonces[i]),
                vm.computeCreateAddress(address(h), nonces[i]),
                "RLP nonce branch mismatch"
            );
        }
    }

    /*//////////////////////////////////////////////////////////////
                                 FUZZ
    //////////////////////////////////////////////////////////////*/

    function testFuzz_createZone_storesParams(
        address adminAddr,
        address seqAddr,
        bytes32 gh,
        bytes32 tgh,
        uint64 tbn
    )
        public
    {
        vm.assume(adminAddr != address(0) && seqAddr != address(0));
        IZoneFactory.CreateZoneParams memory p = _defaultParams();
        p.admin = adminAddr;
        p.sequencer = seqAddr;
        p.zoneParams = ZoneParams(gh, tgh, tbn);

        (uint32 id, address portal) = zoneFactory.createZone(p);
        ZoneInfo memory info = zoneFactory.zones(id);

        assertEq(info.admin, adminAddr);
        assertEq(info.sequencer, seqAddr);
        assertEq(info.genesisBlockHash, gh);
        assertEq(info.genesisTempoBlockHash, tgh);
        assertEq(info.genesisTempoBlockNumber, tbn);
        assertTrue(zoneFactory.isZonePortal(portal));
        assertEq(zoneFactory.zoneCount(), 1);
    }

}

/// @notice A minimal contract that is NOT a TIP-20
contract NotATIP20 {

    function notATIP20Function() external pure returns (bool) {
        return true;
    }

}

/// @notice Harness exposing the internal CREATE-address predictor for boundary testing.
contract ZoneFactoryHarness is ZoneFactory {

    function computeCreateAddress_(address d, uint256 n) external pure returns (address) {
        return _computeCreateAddress(d, n);
    }

}
