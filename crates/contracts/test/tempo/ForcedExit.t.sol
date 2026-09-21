// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    DepositPayload,
    DepositType,
    ENCRYPTION_KEY_GRACE_PERIOD,
    ForcedExit,
    ForcedExitMetadata,
    IZonePortal,
    WithdrawalBounceBackDeposit,
    ZONE_FACTORY_ADDRESS,
    ZONE_MESSENGER_ADDRESS,
    ZONE_VERIFIER_ADDRESS
} from "../../src/runtime/interfaces/IZone.sol";
import { DepositQueueLib } from "../../src/runtime/libraries/DepositQueueLib.sol";
import { ZonePortal } from "../../src/runtime/tempo/ZonePortal.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { Vm } from "forge-std/Vm.sol";
import { StdPrecompiles } from "tempo-std/StdPrecompiles.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";
import { ITIP403Registry } from "tempo-std/interfaces/ITIP403Registry.sol";

/// Internal queue access exists only in the test fixture, never the portal ABI.
contract ForcedExitPortalHarness is ZonePortal {

    function seedTestLegacyQueue(uint64 count, uint64 processed) external {
        depositCount = count;
        lastProcessedDepositNumber = processed;
    }

    function confirmTestDeposits(uint64 number) external {
        lastProcessedDepositNumber = number;
    }

    function enqueueTestBounceBack(address token) external {
        _recordDeposit(
            DepositQueueLib.enqueue(
                currentDepositQueueHash, WithdrawalBounceBackDeposit(token, address(1), 1)
            ),
            MAX_UNPROCESSED_DEPOSITS
        );
    }

}

contract ForcedExitTest is BaseTest {

    ForcedExitPortalHarness portal;
    bytes32 constant G_X = 0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798;
    uint128 constant FEE = 100_000;
    uint64 constant REJECT_ALL_POLICY_ID = 0;
    uint64 constant ALLOW_ALL_POLICY_ID = 1;

    function setUp() public virtual override {
        super.setUp();
        vm.startPrank(pathUSDAdmin);
        pathUSD.grantRole(_ISSUER_ROLE, pathUSDAdmin);
        pathUSD.mint(alice, 1000e6);
        pathUSD.mint(address(zoneGateway), 10e6);
        vm.stopPrank();
        initializePortal();
        vm.prank(admin);
        portal.activateForcedExits();
    }

    function initializePortal() internal {
        portal = new ForcedExitPortalHarness();
        address[] memory sequencers = new address[](1);
        sequencers[0] = sequencer;
        vm.prank(ZONE_FACTORY_ADDRESS);
        portal.initialize(
            1,
            address(pathUSD),
            true,
            true,
            _closedLoopAccounts(),
            _zoneGateways(),
            ZONE_MESSENGER_ADDRESS,
            admin,
            sequencers,
            1,
            ZONE_VERIFIER_ADDRESS,
            ""
        );
        setKey(1);
        vm.prank(alice);
        pathUSD.approve(address(portal), type(uint256).max);
    }

    function setKey(uint256 key) internal {
        Vm.Wallet memory w = vm.createWallet(key);
        bytes32 x = bytes32(w.publicKeyX);
        uint8 parity = w.publicKeyY % 2 == 0 ? 2 : 3;
        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(key, keccak256(abi.encode(address(portal), x, parity)));
        vm.prank(sequencer);
        portal.setSequencerEncryptionKey(x, parity, v, r, s);
    }

    function payload(uint256 length) internal pure returns (DepositPayload memory) {
        return DepositPayload(G_X, 2, new bytes(length), bytes12(uint96(1)), bytes16(uint128(2)));
    }

    function request(uint256 length) internal returns (uint64, uint64) {
        vm.prank(alice);
        return portal.requestForcedExit(address(pathUSD), 0, payload(length));
    }

    function assertUnchanged(uint256 balance, uint256 adminBalance) internal view {
        assertEq(pathUSD.balanceOf(alice), balance);
        assertEq(pathUSD.balanceOf(admin), adminBalance);
        assertEq(pathUSD.balanceOf(address(portal)), 0);
        assertEq(portal.forcedExitCount(), 0);
        assertEq(portal.depositCount(), 0);
        assertEq(portal.currentDepositQueueHash(), bytes32(0));
        (address token, uint64 number) = portal.forcedExitRequests(1);
        assertEq(token, address(0));
        assertEq(number, 0);
    }

    function test_activationDefaultsToZeroAndBlocksAdmission() public {
        initializePortal();
        assertEq(new ZonePortal().forcedExitVersion(), 0);
        assertEq(portal.forcedExitVersion(), 0);
        uint256 balance = pathUSD.balanceOf(alice);
        uint256 adminBalance = pathUSD.balanceOf(admin);
        vm.expectRevert(IZonePortal.ForcedExitsNotActivated.selector);
        request(384);
        assertUnchanged(balance, adminBalance);
        vm.prank(admin);
        vm.expectEmit(address(portal));
        emit IZonePortal.ForcedExitsActivated(1);
        portal.activateForcedExits();
        assertEq(portal.forcedExitVersion(), 1);
        (uint64 id, uint64 number) = request(384);
        assertEq(id, 1);
        assertEq(number, 1);
    }

    function test_activationIsAdminOnlyAndOneWay() public {
        initializePortal();
        vm.prank(alice);
        vm.expectRevert(IZonePortal.NotAdmin.selector);
        portal.activateForcedExits();
        vm.prank(sequencer);
        vm.expectRevert(IZonePortal.NotAdmin.selector);
        portal.activateForcedExits();
        assertEq(portal.forcedExitVersion(), 0);
        vm.prank(admin);
        portal.activateForcedExits();
        vm.prank(admin);
        vm.expectRevert(IZonePortal.ForcedExitsAlreadyActivated.selector);
        portal.activateForcedExits();
        assertEq(portal.forcedExitVersion(), 1);
    }

    function test_feeIdentityQueueAndReconstructibleEvent() public {
        uint256 balance = pathUSD.balanceOf(alice);
        uint256 adminBalance = pathUSD.balanceOf(admin);
        // An ordinary deposit establishes that request ID differs from global queue position.
        vm.prank(alice);
        portal.depositEncrypted(address(pathUSD), 1e6, 0, _depositPayload(alice, 0), alice);
        bytes32 previous = portal.currentDepositQueueHash();
        vm.recordLogs();
        (uint64 id, uint64 number) = request(384);
        assertEq(id, 1);
        assertEq(number, 2);
        ForcedExit memory entry = ForcedExit(
            id,
            address(pathUSD),
            0,
            payload(384),
            alice,
            uint64(block.number),
            uint64(block.timestamp)
        );
        assertEq(
            portal.currentDepositQueueHash(),
            keccak256(abi.encode(DepositType.ForcedExit, entry, previous))
        );
        (address token, uint64 storedNumber) = portal.forcedExitRequests(id);
        assertEq(token, address(pathUSD));
        assertEq(storedNumber, number);
        assertEq(pathUSD.balanceOf(alice), balance - 1e6 - FEE);
        assertEq(pathUSD.balanceOf(admin), adminBalance + FEE);
        assertEq(pathUSD.balanceOf(address(portal)), 1e6);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        Vm.Log memory last = logs[logs.length - 1];
        assertEq(last.emitter, address(portal));
        assertEq(last.topics[0], IZonePortal.ForcedExitRequested.selector);
        assertEq(last.topics[1], bytes32(uint256(number)));
        assertEq(last.data, abi.encode(entry));
        // Ciphertext is deliberately invalid. L1 cannot reject or deduplicate hidden authorization.
        (id, number) = request(384);
        assertEq(id, 2);
        assertEq(number, 3);
    }

    function testFuzz_ciphertextBounds(uint16 rawLength) public {
        uint256 length = bound(uint256(rawLength), 0, 2500);
        if (length < 384 || length > 2368 || length % 32 != 0) {
            vm.expectRevert(
                abi.encodeWithSelector(
                    IZonePortal.InvalidForcedExitCiphertextLength.selector, length
                )
            );
            request(length);
            assertEq(portal.depositCount(), 0);
        } else {
            request(length);
            assertEq(portal.depositCount(), 1);
        }
    }

    function test_lengthEndpointsAndLegacyDepositRule() public {
        request(384);
        request(2368);
        vm.expectRevert(
            abi.encodeWithSelector(IZonePortal.InvalidCiphertextLength.selector, 384, 64)
        );
        vm.prank(alice);
        portal.depositEncrypted(address(pathUSD), 1e6, 0, payload(384), alice);
    }

    function test_pausedPortalAndPausedDeposits() public {
        vm.prank(admin);
        portal.pauseDeposits(address(pathUSD));
        request(384);
        vm.prank(admin);
        portal.pause();
        uint256 balance = pathUSD.balanceOf(alice);
        vm.expectRevert(IZonePortal.PortalIsPaused.selector);
        request(384);
        assertEq(pathUSD.balanceOf(alice), balance);
        assertEq(portal.forcedExitCount(), 1);
    }

    function test_feePayerAccessAndGatewayException() public {
        vm.prank(admin);
        portal.setAllowedAccount(alice, false);
        vm.expectRevert(abi.encodeWithSelector(IZonePortal.AccountNotAllowed.selector, alice));
        request(384);
        vm.startPrank(address(zoneGateway));
        pathUSD.approve(address(portal), FEE);
        portal.requestForcedExit(address(pathUSD), 0, payload(384));
        vm.stopPrank();
        assertEq(portal.forcedExitCount(), 1);
    }

    function test_tokenPointAndKeyValidation() public {
        vm.expectRevert(IZonePortal.TokenNotEnabled.selector);
        vm.prank(alice);
        portal.requestForcedExit(address(token1), 0, payload(384));
        DepositPayload memory p = payload(384);
        p.ephemeralPubkeyYParity = 0;
        vm.expectRevert(IZonePortal.InvalidEphemeralPubkey.selector);
        vm.prank(alice);
        portal.requestForcedExit(address(pathUSD), 0, p);
        p = payload(384);
        p.ephemeralPubkeyX = bytes32(0);
        vm.expectRevert(IZonePortal.InvalidEphemeralPubkey.selector);
        vm.prank(alice);
        portal.requestForcedExit(address(pathUSD), 0, p);
        vm.expectRevert(abi.encodeWithSelector(IZonePortal.InvalidEncryptionKeyIndex.selector, 1));
        vm.prank(alice);
        portal.requestForcedExit(address(pathUSD), 1, payload(384));
        uint64 firstBlock = uint64(block.number);
        uint64 rotatedAt = firstBlock + 1;
        vm.roll(rotatedAt);
        setKey(2);
        vm.roll(uint256(rotatedAt) + ENCRYPTION_KEY_GRACE_PERIOD - 1);
        request(384);
        vm.roll(uint256(rotatedAt) + ENCRYPTION_KEY_GRACE_PERIOD);
        vm.expectRevert(
            abi.encodeWithSelector(
                IZonePortal.EncryptionKeyExpired.selector, 0, firstBlock, rotatedAt
            )
        );
        request(384);
    }

    function test_firstFeeTransferFailureIsAtomic() public {
        vm.prank(alice);
        pathUSD.approve(address(portal), 0);
        uint256 balance = pathUSD.balanceOf(alice);
        uint256 adminBalance = pathUSD.balanceOf(admin);
        vm.expectRevert();
        request(384);
        assertUnchanged(balance, adminBalance);
        vm.prank(alice);
        pathUSD.approve(address(portal), FEE);
        (uint64 id, uint64 number) = request(384);
        assertEq(id, 1);
        assertEq(number, 1);
    }

    function test_blockedAdminRollsBackBothFeeTransfers() public {
        address[] memory blocked = new address[](1);
        blocked[0] = admin;
        uint64 policy = registry.createPolicyWithAccounts(
            address(this), ITIP403Registry.PolicyType.BLACKLIST, blocked
        );
        vm.prank(pathUSDAdmin);
        pathUSD.changeTransferPolicyId(policy);
        uint256 balance = pathUSD.balanceOf(alice);
        uint256 adminBalance = pathUSD.balanceOf(admin);
        vm.expectRevert(IZonePortal.CallbackRejected.selector);
        request(384);
        assertUnchanged(balance, adminBalance);
        registry.modifyPolicyBlacklist(policy, admin, false);
        (uint64 id,) = request(384);
        assertEq(id, 1);
    }

    function test_portalReceivePolicyBlockedPreservesExistingBacking() public {
        // Seed backing through an ordinary deposit before blocking incoming compensation.
        vm.prank(alice);
        portal.depositEncrypted(address(pathUSD), 10e6, 0, _depositPayload(alice, 0), alice);
        uint256 backing = pathUSD.balanceOf(address(portal));
        assertGe(backing, FEE);
        bytes32 queueHash = portal.currentDepositQueueHash();
        uint64 depositCount = portal.depositCount();
        uint256 balance = pathUSD.balanceOf(alice);
        uint256 adminBalance = pathUSD.balanceOf(admin);
        uint256 guardBalance = pathUSD.balanceOf(StdPrecompiles.RECEIVE_POLICY_GUARD_ADDRESS);

        // Model the blocked-recipient state, independent of how it was configured.
        vm.prank(address(portal));
        registry.setReceivePolicy(REJECT_ALL_POLICY_ID, ALLOW_ALL_POLICY_ID, address(0));
        vm.expectRevert(IZonePortal.CallbackRejected.selector);
        request(384);
        assertEq(pathUSD.balanceOf(alice), balance);
        assertEq(pathUSD.balanceOf(admin), adminBalance);
        assertEq(pathUSD.balanceOf(address(portal)), backing);
        assertEq(pathUSD.balanceOf(StdPrecompiles.RECEIVE_POLICY_GUARD_ADDRESS), guardBalance);
        assertEq(portal.depositCount(), depositCount);
        assertEq(portal.currentDepositQueueHash(), queueHash);
        assertEq(portal.forcedExitCount(), 0);
        (address token, uint64 number) = portal.forcedExitRequests(1);
        assertEq(token, address(0));
        assertEq(number, 0);

        vm.prank(address(portal));
        registry.setReceivePolicy(ALLOW_ALL_POLICY_ID, ALLOW_ALL_POLICY_ID, address(0));
        (uint64 id, uint64 admittedNumber) = request(384);
        assertEq(id, 1);
        assertEq(admittedNumber, depositCount + 1);
        assertEq(pathUSD.balanceOf(alice), balance - FEE);
        assertEq(pathUSD.balanceOf(admin), adminBalance + FEE);
        assertEq(pathUSD.balanceOf(address(portal)), backing);
    }

    function test_adminReceivePolicyBlockedRollsBackAdmission() public {
        vm.prank(admin);
        registry.setReceivePolicy(REJECT_ALL_POLICY_ID, ALLOW_ALL_POLICY_ID, address(0));

        uint256 balance = pathUSD.balanceOf(alice);
        uint256 adminBalance = pathUSD.balanceOf(admin);
        uint256 guardBalance = pathUSD.balanceOf(StdPrecompiles.RECEIVE_POLICY_GUARD_ADDRESS);
        vm.expectRevert(IZonePortal.CallbackRejected.selector);
        request(384);
        assertUnchanged(balance, adminBalance);
        assertEq(pathUSD.balanceOf(StdPrecompiles.RECEIVE_POLICY_GUARD_ADDRESS), guardBalance);

        vm.prank(admin);
        registry.setReceivePolicy(ALLOW_ALL_POLICY_ID, ALLOW_ALL_POLICY_ID, address(0));
        (uint64 id, uint64 number) = request(384);
        assertEq(id, 1);
        assertEq(number, 1);
        assertEq(pathUSD.balanceOf(admin), adminBalance + FEE);
    }

    function test_pausedTokenRollsBackAdmission() public {
        vm.startPrank(pathUSDAdmin);
        pathUSD.grantRole(_PAUSE_ROLE, pathUSDAdmin);
        pathUSD.pause();
        vm.stopPrank();
        uint256 balance = pathUSD.balanceOf(alice);
        uint256 adminBalance = pathUSD.balanceOf(admin);
        vm.expectRevert(ITIP20.ContractPaused.selector);
        request(384);
        assertUnchanged(balance, adminBalance);
    }

    function test_packedActivationAndRequestsPreserveProverFields() public {
        initializePortal();
        uint256 cursor = uint256(7) | (uint256(1) << 64);
        vm.store(address(portal), bytes32(uint256(28)), bytes32(cursor));
        vm.prank(admin);
        portal.activateForcedExits();
        request(384);
        assertEq(portal.lastProcessedEnabledTokenCount(), 7);
        assertTrue(portal.tokenEnablementCursorInitialized());
        assertEq(portal.forcedExitVersion(), 1);
        assertEq(portal.forcedExitCount(), 1);
        assertEq(
            uint256(vm.load(address(portal), bytes32(uint256(28)))),
            cursor | (uint256(1) << 72) | (uint256(1) << 136)
        );
        bytes32 metadataSlot = keccak256(abi.encode(uint64(1), uint256(29)));
        assertEq(
            uint256(vm.load(address(portal), metadataSlot)),
            uint256(uint160(address(pathUSD))) | (uint256(1) << 160)
        );
    }

    function test_legacyOutstandingQueueNeedsNoWeightMigration() public {
        // An upgraded portal has no extra-weight checkpoints for existing deposits.
        portal.seedTestLegacyQueue(300, 100);
        uint64 maximum = portal.MAX_UNPROCESSED_DEPOSITS() - 20;
        vm.expectRevert(
            abi.encodeWithSelector(IZonePortal.DepositBlockCapacityExceeded.selector, maximum)
        );
        request(384);
        portal.confirmTestDeposits(104);
        (, uint64 number) = request(384);
        assertEq(number, 301);
        vm.expectRevert(
            abi.encodeWithSelector(IZonePortal.DepositBlockCapacityExceeded.selector, maximum)
        );
        request(384);
        portal.confirmTestDeposits(301);
        request(384);
    }

    function test_sharedCapacityRetainsBounceBackReserve() public {
        uint64 maximum = portal.MAX_UNPROCESSED_DEPOSITS() - 20;
        vm.prank(alice);
        portal.depositEncrypted(address(pathUSD), 1e6, 0, _depositPayload(alice, 0), alice);
        uint64 weight = portal.FORCED_EXIT_ADMISSION_WEIGHT();
        uint64 requests = (maximum - 1) / weight;
        for (uint256 i; i < requests; ++i) {
            request(384);
        }
        // Fill the remaining shared units with ordinary deposits.
        uint64 ordinary = maximum - requests * weight;
        for (uint256 i = 1; i < ordinary; ++i) {
            vm.prank(alice);
            portal.depositEncrypted(address(pathUSD), 1e6, 0, _depositPayload(alice, 0), alice);
        }
        uint256 balance = pathUSD.balanceOf(alice);
        bytes32 hash = portal.currentDepositQueueHash();
        vm.expectRevert(
            abi.encodeWithSelector(IZonePortal.DepositBlockCapacityExceeded.selector, maximum)
        );
        request(384);
        vm.expectRevert(
            abi.encodeWithSelector(IZonePortal.DepositBlockCapacityExceeded.selector, maximum)
        );
        vm.prank(alice);
        portal.depositEncrypted(address(pathUSD), 1e6, 0, _depositPayload(alice, 0), alice);
        assertEq(pathUSD.balanceOf(alice), balance);
        assertEq(portal.currentDepositQueueHash(), hash);
        assertEq(portal.forcedExitCount(), requests);
        for (uint256 i = 0; i < 20; ++i) {
            portal.enqueueTestBounceBack(address(pathUSD));
        }
        assertEq(portal.depositCount(), requests + ordinary + 20);
        vm.roll(block.number + 1);
        vm.expectRevert(
            abi.encodeWithSelector(IZonePortal.DepositBlockCapacityExceeded.selector, maximum)
        );
        request(384);
        // Processing one ordinary and one forced entry frees 15 units, but
        // the 20 reserved units still prevent another public admission.
        portal.confirmTestDeposits(2);
        vm.expectRevert(
            abi.encodeWithSelector(IZonePortal.DepositBlockCapacityExceeded.selector, maximum)
        );
        request(384);
        portal.confirmTestDeposits(4);
        request(384);
        assertEq(portal.forcedExitCount(), requests + 1);
        portal.confirmTestDeposits(portal.depositCount());
        request(384);
        assertEq(portal.forcedExitCount(), requests + 2);
    }

}
