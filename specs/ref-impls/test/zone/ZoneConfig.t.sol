// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    IZoneConfig,
    IZoneFactory,
    IZonePortal,
    PORTAL_ENCRYPTION_KEYS_SLOT,
    PORTAL_PENDING_SEQUENCER_SLOT,
    PORTAL_SEQUENCER_SLOT,
    PORTAL_TOKEN_CONFIGS_SLOT,
    ZoneParams
} from "../../src/zone/IZone.sol";
import { ZoneConfig } from "../../src/zone/ZoneConfig.sol";
import { ZoneFactory } from "../../src/zone/ZoneFactory.sol";
import { ZonePortal } from "../../src/zone/ZonePortal.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { MockTempoState } from "./mocks/MockTempoState.sol";
import { Vm } from "forge-std/Vm.sol";

contract ZoneConfigTest is BaseTest {

    ZoneFactory public zoneFactory;
    ZonePortal public portal;
    ZoneConfig public config;
    MockTempoState public tempoState;

    bytes32 public constant GENESIS_BLOCK_HASH = keccak256("genesis");
    bytes32 public constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 public genesisTempoBlockNumber;

    function setUp() public override {
        super.setUp();

        zoneFactory = new ZoneFactory();
        genesisTempoBlockNumber = uint64(block.number);

        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            initialToken: address(pathUSD),
            admin: admin,
            sequencer: admin,
            verifier: zoneFactory.verifier(),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: genesisTempoBlockNumber
            }),
            rpcUrl: "https://rpc.test-zone.example"
        });

        (, address portalAddr) = zoneFactory.createZone(params);
        portal = ZonePortal(portalAddr);
        tempoState = new MockTempoState(admin, GENESIS_TEMPO_BLOCK_HASH, genesisTempoBlockNumber);
        config = new ZoneConfig(address(portal), address(tempoState));

        _syncPortalSlot(PORTAL_SEQUENCER_SLOT);
        _syncPortalSlot(PORTAL_PENDING_SEQUENCER_SLOT);
        _syncTokenConfig(address(pathUSD));
    }

    function _syncPortalSlot(bytes32 slot) internal {
        tempoState.setMockStorageValue(address(portal), slot, vm.load(address(portal), slot));
    }

    function _syncTokenConfig(address token) internal {
        bytes32 slot = keccak256(abi.encode(token, PORTAL_TOKEN_CONFIGS_SLOT));
        tempoState.setMockStorageValue(address(portal), slot, vm.load(address(portal), slot));
    }

    function _syncEncryptionKeys(uint256 count) internal {
        _syncPortalSlot(PORTAL_ENCRYPTION_KEYS_SLOT);
        uint256 base = uint256(keccak256(abi.encode(uint256(PORTAL_ENCRYPTION_KEYS_SLOT))));
        for (uint256 i = 0; i < count; i++) {
            bytes32 slotX = bytes32(base + (i * 2));
            bytes32 slotMeta = bytes32(base + (i * 2) + 1);
            _syncPortalSlot(slotX);
            _syncPortalSlot(slotMeta);
        }
    }

    function _setEncKeyWithPoP(uint256 privateKey) internal returns (bytes32 x, uint8 yParity) {
        Vm.Wallet memory w = vm.createWallet(privateKey);
        x = bytes32(w.publicKeyX);
        yParity = w.publicKeyY % 2 == 0 ? 0x02 : 0x03;
        bytes32 message = keccak256(abi.encode(address(portal), x, yParity));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(w.privateKey, message);
        portal.setSequencerEncryptionKey(x, yParity, v, r, s);
    }

    /// @notice Verifies the config reads the current sequencer from the portal.
    function test_sequencer_returnsPortalSequencer() public view {
        assertEq(config.sequencer(), portal.sequencer());
    }

    /// @notice Verifies sequencer membership is true for admin and false otherwise.
    function test_isSequencer_trueAndFalse() public view {
        assertTrue(config.isSequencer(admin));
        assertFalse(config.isSequencer(alice));
    }

    /// @notice Verifies enabled token status is true for the initial token only.
    function test_isEnabledToken_trueAndFalse() public view {
        assertTrue(config.isEnabledToken(address(pathUSD)));
        assertFalse(config.isEnabledToken(address(token1)));
    }

    /// @notice Verifies reading the sequencer encryption key reverts before any key is set.
    function test_sequencerEncryptionKey_revertsWhenNoneSet() public {
        vm.expectRevert(IZoneConfig.NoEncryptionKeySet.selector);
        config.sequencerEncryptionKey();
    }

    /// @notice Verifies the config returns the latest sequencer encryption key.
    function test_sequencerEncryptionKey_returnsLatestKey() public {
        _setEncKeyWithPoP(1);
        vm.roll(block.number + 1);
        (bytes32 x, uint8 yParity) = _setEncKeyWithPoP(2);
        _syncEncryptionKeys(2);

        (bytes32 storedX, uint8 storedYParity) = config.sequencerEncryptionKey();
        assertEq(storedX, x);
        assertEq(storedYParity, yParity);
    }

    /// @notice Verifies the config reads the pending sequencer from portal storage.
    function test_pendingSequencer() public {
        vm.prank(admin);
        portal.transferSequencer(alice);
        _syncPortalSlot(PORTAL_PENDING_SEQUENCER_SLOT);

        assertEq(config.pendingSequencer(), alice);
    }

    /// @notice Verifies sequencer slot reads stay correct after sequencer rotation.
    function test_storageSlotRegression_readsSequencerAfterRotation() public {
        vm.prank(admin);
        portal.transferSequencer(alice);
        vm.prank(alice);
        portal.acceptSequencer();
        _syncPortalSlot(PORTAL_SEQUENCER_SLOT);
        _syncPortalSlot(PORTAL_PENDING_SEQUENCER_SLOT);

        assertEq(portal.sequencer(), alice);
        assertEq(config.sequencer(), portal.sequencer());
        assertEq(config.pendingSequencer(), address(0));
    }

}
