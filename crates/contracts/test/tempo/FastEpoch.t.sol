// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    BlockTransition,
    DepositQueueTransition,
    FastBarrierResolution,
    FastBarrierStatement,
    FastCheckpointStatement,
    FastEpochConfig,
    FastPeerBarrier,
    FastProofMode,
    IZoneFactory,
    IZonePortal,
    TokenEnablementTransition,
    ZONE_FACTORY_ADDRESS
} from "../../src/runtime/interfaces/IZone.sol";
import { getBlockHash } from "../../src/runtime/libraries/BlockHashHistory.sol";
import { ZonePortal } from "../../src/runtime/tempo/ZonePortal.sol";
import { BaseTest } from "../BaseTest.t.sol";

/// @notice T14 acceptance tests derived from the instant-transfer epoch and settlement rules.
contract FastEpochTest is BaseTest {

    uint256 internal constant MEMBER_A_KEY = 0xA11CE;
    uint256 internal constant MEMBER_B_KEY = 0xB0B;
    uint256 internal constant MEMBER_C_KEY = 0xCA401;
    uint256 internal constant OUTSIDER_KEY = 0xBAD;

    uint64 internal constant EPOCH = 14;
    uint32 internal constant PROTOCOL_VERSION = 1;

    bytes32 internal constant EIP712_DOMAIN_TYPEHASH = keccak256(
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
    );
    bytes32 internal constant FAST_SETTLEMENT_TYPEHASH = keccak256(
        "FastSettlementAttestation(uint32 zoneId,uint64 fastEpoch,bytes32 rosterHash,uint256 previousZoneHeight,bytes32 previousBlockHash,uint64 previousWithdrawalBatchIndex,uint256 zoneHeight,uint256 withdrawalBatchIndex,address verifier,uint64 tempoBlockNumber,uint64 anchorBlockNumber,bytes32 anchorBlockHash,bytes32 blockTransitionHash,bytes32 depositQueueTransitionHash,bytes32 tokenEnablementTransitionHash,bytes32 withdrawalQueueHash,bytes32 verifierConfigHash)"
    );
    bytes32 internal constant LEGACY_SETTLEMENT_TYPEHASH = keccak256(
        "SettlementAttestation(uint32 zoneId,uint64 sequencerSetVersion,uint256 zoneHeight,uint256 withdrawalBatchIndex,address verifier,uint64 tempoBlockNumber,uint64 anchorBlockNumber,bytes32 anchorBlockHash,bytes32 blockTransitionHash,bytes32 depositQueueTransitionHash,bytes32 tokenEnablementTransitionHash,bytes32 withdrawalQueueHash,bytes32 verifierConfigHash)"
    );
    bytes32 internal constant ROSTER_TAG = keccak256("TEMPO_ZONE_FAST_ROSTER_T14_V1");
    bytes32 internal constant BARRIER_TAG = keccak256("TEMPO_ZONE_FAST_BARRIER_T14_V1");
    bytes32 internal constant RESOLUTION_TAG =
        keccak256("TEMPO_ZONE_FAST_BARRIER_RESOLUTION_T14_V1");
    bytes32 internal constant FINAL_SETTLEMENT_TAG =
        keccak256("TEMPO_ZONE_FAST_FINAL_SETTLEMENT_T14_V1");
    bytes32 internal constant CHECKPOINT_TAG = keccak256("TEMPO_ZONE_FAST_CHECKPOINT_T14_V1");
    bytes32 internal constant EMPTY_UNRESOLVED_ROOT =
        keccak256("TEMPO_ZONE_FAST_EMPTY_UNRESOLVED_T14_V1");

    ZonePortal internal portal;
    address[] internal members;
    address[] internal peers;
    bytes32 internal verifierCodeHash;

    struct FastBatch {
        uint64 tempoBlockNumber;
        BlockTransition blockTransition;
        DepositQueueTransition depositTransition;
        TokenEnablementTransition tokenTransition;
        bytes32 withdrawalQueueHash;
        bytes verifierConfig;
        uint256 zoneHeight;
    }

    function setUp() public override {
        super.setUp();

        address[] memory initialSequencers = new address[](1);
        initialSequencers[0] = sequencer;
        portal = _createZonePortal(
            14, address(pathUSD), admin, initialSequencers, 1, "https://t14.invalid"
        );
        verifierCodeHash = portal.verifier().codehash;

        members = _memberSet(MEMBER_A_KEY, MEMBER_B_KEY, MEMBER_C_KEY);
        peers = _peerSet();
        for (uint256 i; i < peers.length; ++i) {
            vm.mockCall(
                ZONE_FACTORY_ADDRESS,
                abi.encodeCall(IZoneFactory.isZonePortal, (peers[i])),
                abi.encode(true)
            );
            for (uint256 j; j < members.length; ++j) {
                vm.mockCall(
                    peers[i],
                    abi.encodeCall(IZonePortal.isFastEpochMember, (EPOCH, members[j])),
                    abi.encode(true)
                );
            }
        }
    }

    function test_configureFastEpochRequiresExactlyThreeMembersAndNinePeers() public {
        address[] memory twoMembers = new address[](2);
        twoMembers[0] = members[0];
        twoMembers[1] = members[1];
        _expectInvalidConfiguration(twoMembers, peers);

        address[] memory fourMembers = new address[](4);
        for (uint256 i; i < members.length; ++i) {
            fourMembers[i] = members[i];
        }
        fourMembers[3] = vm.addr(OUTSIDER_KEY);
        _expectInvalidConfiguration(fourMembers, peers);

        address[] memory eightPeers = new address[](8);
        for (uint256 i; i < eightPeers.length; ++i) {
            eightPeers[i] = peers[i];
        }
        _expectInvalidConfiguration(members, eightPeers);

        address[] memory tenPeers = new address[](10);
        for (uint256 i; i < peers.length; ++i) {
            tenPeers[i] = peers[i];
        }
        tenPeers[9] = address(0x9010);
        vm.mockCall(
            ZONE_FACTORY_ADDRESS,
            abi.encodeCall(IZoneFactory.isZonePortal, (tenPeers[9])),
            abi.encode(true)
        );
        _expectInvalidConfiguration(members, tenPeers);

        _activate(EPOCH, members, peers);
        assertEq(portal.fastEpochMemberCount(EPOCH), 3);
        assertEq(portal.fastEpochPeerCount(EPOCH), 9);
        assertEq(portal.fastEpochConfig(EPOCH).threshold, 2);
    }

    function test_configureFastEpochRejectsDuplicateMembersAndPeers() public {
        address[] memory duplicateMembers = _copy(members);
        duplicateMembers[2] = duplicateMembers[0];
        _expectInvalidConfiguration(duplicateMembers, peers);

        address[] memory duplicatePeers = _copy(peers);
        duplicatePeers[8] = duplicatePeers[0];
        _expectInvalidConfiguration(members, duplicatePeers);
    }

    function test_closeDoesNotReopenLegacyAuthority() public {
        _activate(EPOCH, members, peers);
        _factoryCall(abi.encodeCall(IZonePortal.closeFastEpoch, (EPOCH, keccak256("closure"))));

        assertTrue(portal.fastEpochActive(), "closed epoch remains the active authority");
        uint64 legacyLeaderEpoch = portal.leaderEpoch();
        vm.prank(admin);
        vm.expectRevert(abi.encodeWithSelector(IZonePortal.FastEpochActive.selector, EPOCH));
        portal.setLeader(members[0], legacyLeaderEpoch);

        address[] memory replacement = new address[](1);
        replacement[0] = sequencer;
        vm.prank(admin);
        vm.expectRevert(abi.encodeWithSelector(IZonePortal.FastEpochActive.selector, EPOCH));
        portal.setSequencerSet(replacement, 1);
    }

    function test_fastSettlementRejectsOneDuplicateAndNonmemberSignatures() public {
        _activate(EPOCH, members, peers);
        FastBatch memory batch = _batch("quorum-boundaries");
        bytes32 digest = _fastDigest(batch, EPOCH, _rosterHash(portal, EPOCH, members, peers));

        bytes[] memory one = new bytes[](1);
        one[0] = _sign(MEMBER_A_KEY, digest);
        _expectInvalidCertificate(batch, one);

        bytes[] memory duplicate = new bytes[](2);
        duplicate[0] = _sign(MEMBER_A_KEY, digest);
        duplicate[1] = duplicate[0];
        _expectInvalidCertificate(batch, duplicate);

        bytes[] memory nonmember = new bytes[](2);
        nonmember[0] = _sign(MEMBER_A_KEY, digest);
        nonmember[1] = _sign(OUTSIDER_KEY, digest);
        _expectInvalidCertificate(batch, nonmember);
    }

    function test_fastSettlementRejectsOldDomainMixedEpochAndMixedRoster() public {
        _activate(EPOCH, members, peers);
        FastBatch memory batch = _batch("domain-boundaries");
        bytes32 rosterHash = _rosterHash(portal, EPOCH, members, peers);
        bytes32 current = _fastDigest(batch, EPOCH, rosterHash);

        bytes[] memory oldDomain =
            _pair(MEMBER_A_KEY, MEMBER_B_KEY, _legacyDigest(batch), _legacyDigest(batch));
        _expectInvalidCertificate(batch, oldDomain);

        bytes[] memory mixedEpoch =
            _pair(MEMBER_A_KEY, MEMBER_B_KEY, current, _fastDigest(batch, EPOCH - 1, rosterHash));
        _expectInvalidCertificate(batch, mixedEpoch);

        bytes[] memory mixedRoster = _pair(
            MEMBER_A_KEY,
            MEMBER_B_KEY,
            current,
            _fastDigest(batch, EPOCH, keccak256("different-roster"))
        );
        _expectInvalidCertificate(batch, mixedRoster);
    }

    function test_fastSettlementRejectsWrongCommittedPrefix() public {
        _activate(EPOCH, members, peers);
        FastBatch memory batch = _batch("prefix-boundary");
        bytes32 rosterHash = _rosterHash(portal, EPOCH, members, peers);
        bytes32 wrongPrefixDigest = _fastDigestForPrefix(
            batch,
            EPOCH,
            rosterHash,
            portal.zoneHeight(),
            keccak256("unaccepted-parent"),
            portal.withdrawalBatchIndex()
        );
        bytes[] memory signatures =
            _pair(MEMBER_A_KEY, MEMBER_B_KEY, wrongPrefixDigest, wrongPrefixDigest);
        _expectInvalidCertificate(batch, signatures);
    }

    function test_fastSettlementAcceptsEveryTwoOfThreePair() public {
        _activate(EPOCH, members, peers);
        uint256 snapshot = vm.snapshotState();

        _settleWith(MEMBER_A_KEY, MEMBER_B_KEY, "ab");
        assertTrue(vm.revertToState(snapshot));
        _settleWith(MEMBER_A_KEY, MEMBER_C_KEY, "ac");
        assertTrue(vm.revertToState(snapshot));
        _settleWith(MEMBER_B_KEY, MEMBER_C_KEY, "bc");
    }

    function test_emptyBarrierIsCountedOnceAndDuplicateRejected() public {
        _activateAndClose();
        _recordBarrier(peers[0], EMPTY_UNRESOLVED_ROOT, 0);
        FastPeerBarrier memory barrier = portal.fastPeerBarrier(EPOCH, peers[0]);
        assertTrue(barrier.recorded);
        assertTrue(barrier.completeLockRoot != bytes32(0));
        assertEq(barrier.unresolvedRoot, EMPTY_UNRESOLVED_ROOT);
        assertEq(portal.fastEpochConfig(EPOCH).recordedPeerBarriers, 1);

        vm.expectRevert(
            abi.encodeWithSelector(IZonePortal.FastPeerBarrierAlreadyRecorded.selector, peers[0])
        );
        _recordBarrier(peers[0], EMPTY_UNRESOLVED_ROOT, 0);
        assertEq(portal.fastEpochConfig(EPOCH).recordedPeerBarriers, 1);
    }

    function test_barrierRejectsWrongPeer() public {
        _activateAndClose();
        address wrongPeer = address(0xDEAD);
        vm.expectRevert(IZonePortal.InvalidFastEpoch.selector);
        _recordBarrier(wrongPeer, EMPTY_UNRESOLVED_ROOT, 0);
    }

    function test_barrierRejectsUnsignedOwnerRootsWrongDomainSignerAndReplay() public {
        _activateAndClose();
        FastBarrierStatement memory statement =
            _barrierStatement(peers[0], EMPTY_UNRESOLVED_ROOT, 0);
        bytes32 digest = keccak256(abi.encode(BARRIER_TAG, block.chainid, statement));

        vm.expectRevert(IZonePortal.InvalidFastCertificate.selector);
        _factoryCall(
            abi.encodeCall(IZonePortal.recordFastPeerBarrier, (statement, new bytes[](0)))
        );

        bytes32 wrongDomain = keccak256(abi.encode(keccak256("wrong-domain"), block.chainid, statement));
        vm.expectRevert(IZonePortal.InvalidFastCertificate.selector);
        _factoryCall(
            abi.encodeCall(
                IZonePortal.recordFastPeerBarrier,
                (statement, _pair(MEMBER_A_KEY, MEMBER_B_KEY, wrongDomain, wrongDomain))
            )
        );

        vm.expectRevert(IZonePortal.InvalidFastCertificate.selector);
        _factoryCall(
            abi.encodeCall(
                IZonePortal.recordFastPeerBarrier,
                (statement, _pair(MEMBER_A_KEY, OUTSIDER_KEY, digest, digest))
            )
        );

        FastBarrierStatement memory fabricated = statement;
        fabricated.completeLockRoot = keccak256("owner-fabricated-root");
        vm.expectRevert(IZonePortal.InvalidFastCertificate.selector);
        _factoryCall(
            abi.encodeCall(
                IZonePortal.recordFastPeerBarrier,
                (fabricated, _pair(MEMBER_A_KEY, MEMBER_B_KEY, digest, digest))
            )
        );

        statement.sourcePortal = peers[1];
        vm.expectRevert(IZonePortal.InvalidFastCertificate.selector);
        _factoryCall(
            abi.encodeCall(
                IZonePortal.recordFastPeerBarrier,
                (statement, _pair(MEMBER_A_KEY, MEMBER_B_KEY, digest, digest))
            )
        );
    }

    function test_proofModeIsExplicitAndProofRequiredRejectsPrototype() public {
        bytes32 rosterHash = _rosterHash(portal, EPOCH, members, peers);
        vm.expectRevert(IZonePortal.InvalidFastEpoch.selector);
        _factoryCall(
            abi.encodeCall(
                IZonePortal.configureFastEpoch,
                (
                    EPOCH,
                    PROTOCOL_VERSION,
                    FastProofMode.Unset,
                    verifierCodeHash,
                    keccak256(""),
                    members,
                    peers,
                    rosterHash
                )
            )
        );

        bytes32 proofRosterHash = keccak256(
            abi.encode(
                ROSTER_TAG,
                address(portal),
                EPOCH,
                PROTOCOL_VERSION,
                uint8(2),
                FastProofMode.ProofRequired,
                verifierCodeHash,
                keccak256("proof-config"),
                members,
                peers
            )
        );
        vm.expectRevert(IZonePortal.InvalidFastProofConfiguration.selector);
        _factoryCall(
            abi.encodeCall(
                IZonePortal.configureFastEpoch,
                (
                    EPOCH,
                    PROTOCOL_VERSION,
                    FastProofMode.ProofRequired,
                    verifierCodeHash,
                    keccak256("proof-config"),
                    members,
                    peers,
                    proofRosterHash
                )
            )
        );

        vm.expectRevert(IZonePortal.InvalidFastProofConfiguration.selector);
        _factoryCall(
            abi.encodeCall(
                IZonePortal.configureFastEpoch,
                (
                    EPOCH,
                    PROTOCOL_VERSION,
                    FastProofMode.OperatorAttested,
                    keccak256("wrong-code"),
                    keccak256(""),
                    members,
                    peers,
                    rosterHash
                )
            )
        );
    }

    function test_missingPeerAndLateUnresolvedLockPreventRetirement() public {
        _activateAndClose();
        for (uint256 i; i < peers.length - 1; ++i) {
            _recordAndFinalizeBarrier(i, EMPTY_UNRESOLVED_ROOT, 0);
        }

        _expectNotDrainedRetirement(8, 9);

        bytes32 delayedLock = keccak256("delayed-valid-lock");
        _recordBarrier(peers[8], delayedLock, 1);
        bytes32 delayedBarrierHash = portal.fastPeerBarrier(EPOCH, peers[8]).barrierHash;
        vm.expectRevert(IZonePortal.InvalidFastCertificate.selector);
        _factoryCall(
            abi.encodeCall(
                IZonePortal.finalizeFastPeerBarrier,
                (
                    EPOCH,
                    peers[8],
                    FastBarrierResolution({
                        barrierHash: delayedBarrierHash,
                        terminalRoot: keccak256("partial-terminal"),
                        dispositionRoot: keccak256("partial-disposition"),
                        resolvedCount: 0,
                        remainingUnresolvedRoot: delayedLock,
                        remainingUnresolvedCount: 1
                    }),
                    new bytes[](0)
                )
            )
        );
        _expectNotDrainedRetirement(8, 9);

        _finalizeBarrier(peers[8]);
        _recordFinalSettlement();
        _installCheckpoint(EPOCH + 1);
        _factoryCall(abi.encodeCall(IZonePortal.retireFastEpoch, (EPOCH)));
        assertTrue(portal.fastEpochConfig(EPOCH).retired);
    }

    function test_finalSettlementFreezesTheAcceptedPrefix() public {
        _activateAndClose();
        _finalizeAllBarriers();

        uint256 futureHeight = portal.zoneHeight() + 1;
        uint64 currentWithdrawalBatchIndex = portal.withdrawalBatchIndex();
        vm.expectRevert();
        _factoryCall(
            abi.encodeCall(
                IZonePortal.recordFastFinalSettlement,
                (EPOCH, futureHeight, keccak256("future-tip"), currentWithdrawalBatchIndex, new bytes[](0))
            )
        );

        _recordFinalSettlement();
        FastBatch memory later = _batch("late-settlement");
        bytes32 digest = _fastDigest(later, EPOCH, _rosterHash(portal, EPOCH, members, peers));
        bytes[] memory signatures = _pair(MEMBER_A_KEY, MEMBER_B_KEY, digest, digest);
        vm.prank(members[0]);
        vm.expectRevert(IZonePortal.InvalidFastEpoch.selector);
        _submit(later, signatures);

        uint256 settledHeight = portal.zoneHeight();
        bytes32 settledBlockHash = portal.blockHash();
        currentWithdrawalBatchIndex = portal.withdrawalBatchIndex();
        vm.expectRevert();
        _factoryCall(
            abi.encodeCall(
                IZonePortal.recordFastFinalSettlement,
                (EPOCH, settledHeight, settledBlockHash, currentWithdrawalBatchIndex, new bytes[](0))
            )
        );
    }

    function test_nextCheckpointActivatesOnlyAfterRetirementAndRetainsHistoricalKeys() public {
        _activateAndClose();
        _finalizeAllBarriers();
        _recordFinalSettlement();
        bytes32 checkpoint = _installCheckpoint(EPOCH + 1);

        address[] memory nextMembers = _memberSet(0x101, 0x102, 0x103);
        vm.expectRevert(abi.encodeWithSelector(IZonePortal.FastEpochActive.selector, EPOCH));
        _activate(EPOCH + 1, nextMembers, peers);

        _factoryCall(abi.encodeCall(IZonePortal.retireFastEpoch, (EPOCH)));
        _activate(EPOCH + 1, nextMembers, peers);

        assertEq(portal.fastEpoch(), EPOCH + 1);
        assertEq(portal.fastEpochConfig(EPOCH).checkpointHash, checkpoint);
        assertTrue(portal.fastEpochConfig(EPOCH).retired);
        assertEq(portal.fastEpochMemberCount(EPOCH), 3);
        assertEq(portal.fastEpochMemberAt(EPOCH, 0), members[0]);
        assertEq(portal.fastEpochMemberAt(EPOCH, 1), members[1]);
        assertEq(portal.fastEpochMemberAt(EPOCH, 2), members[2]);
        assertTrue(portal.isFastEpochMember(EPOCH, members[0]));
        assertEq(portal.fastEpochPeerCount(EPOCH), 9);
        assertTrue(portal.fastPeerBarrier(EPOCH, peers[0]).finalized);
    }

    function test_checkpointRequiresSignedAcceptedFinalPrefixWithoutMutation() public {
        _activateAndClose();
        _finalizeAllBarriers();
        _recordFinalSettlement();
        address[] memory nextMembers = _memberSet(0x101, 0x102, 0x103);
        FastCheckpointStatement memory statement = _checkpointStatement(EPOCH + 1, nextMembers);
        assertEq(statement.finalZoneHeight, 0, "exercise the accepted genesis prefix");
        assertTrue(statement.checkpointLogIndex > 0, "genesis prefix still requires a Raft entry");
        bytes32 digest = keccak256(abi.encode(CHECKPOINT_TAG, block.chainid, statement));

        vm.expectRevert(IZonePortal.InvalidFastCertificate.selector);
        _factoryCall(
            abi.encodeCall(
                IZonePortal.installFastCheckpoint, (statement, nextMembers, new bytes[](0))
            )
        );

        vm.expectRevert(IZonePortal.InvalidFastCertificate.selector);
        _factoryCall(
            abi.encodeCall(
                IZonePortal.installFastCheckpoint,
                (statement, nextMembers, _pair(0x101, OUTSIDER_KEY, digest, digest))
            )
        );

        FastCheckpointStatement memory laterHead = _checkpointStatement(EPOCH + 1, nextMembers);
        laterHead.checkpointHeight = statement.finalZoneHeight + 1;
        laterHead.checkpointBlockHash = keccak256("later-unsettled-head");
        _expectRejectedCheckpoint(laterHead, nextMembers);

        FastCheckpointStatement memory substitutedHash =
            _checkpointStatement(EPOCH + 1, nextMembers);
        substitutedHash.checkpointBlockHash = keccak256("substituted-checkpoint-hash");
        _expectRejectedCheckpoint(substitutedHash, nextMembers);

        FastCheckpointStatement memory substitutedHeight =
            _checkpointStatement(EPOCH + 1, nextMembers);
        substitutedHeight.checkpointHeight = statement.finalZoneHeight + 1;
        _expectRejectedCheckpoint(substitutedHeight, nextMembers);

        FastCheckpointStatement memory emptyLogPrefix =
            _checkpointStatement(EPOCH + 1, nextMembers);
        emptyLogPrefix.checkpointLogIndex = 0;
        _expectRejectedCheckpoint(emptyLogPrefix, nextMembers);

        digest = keccak256(abi.encode(CHECKPOINT_TAG, block.chainid, statement));
        _factoryCall(
            abi.encodeCall(
                IZonePortal.installFastCheckpoint,
                (statement, nextMembers, _pair(0x101, 0x102, digest, digest))
            )
        );

        FastEpochConfig memory installed = portal.fastEpochConfig(EPOCH);
        assertEq(installed.checkpointLogIndex, 101);
        assertEq(installed.checkpointHeight, installed.finalSettlementHeight);
        assertEq(installed.checkpointBlockHash, installed.finalSettlementBlockHash);
        assertTrue(installed.checkpointStateRoot != bytes32(0));
        assertTrue(installed.checkpointHash != bytes32(0));
    }

    function _activateAndClose() internal {
        _activate(EPOCH, members, peers);
        _factoryCall(abi.encodeCall(IZonePortal.closeFastEpoch, (EPOCH, keccak256("closure"))));
    }

    function _activate(uint64 epoch, address[] memory roster, address[] memory peerSet) internal {
        _factoryCall(
            abi.encodeCall(
                IZonePortal.configureFastEpoch,
                (
                    epoch,
                    PROTOCOL_VERSION,
                    FastProofMode.OperatorAttested,
                    verifierCodeHash,
                    keccak256(""),
                    roster,
                    peerSet,
                    _rosterHash(portal, epoch, roster, peerSet)
                )
            )
        );
    }

    function _expectInvalidConfiguration(
        address[] memory roster,
        address[] memory peerSet
    )
        internal
    {
        vm.expectRevert(IZonePortal.InvalidFastEpoch.selector);
        _activate(EPOCH, roster, peerSet);
    }

    function _factoryCall(bytes memory callData) internal {
        vm.prank(ZONE_FACTORY_ADDRESS);
        (bool success, bytes memory revertData) = address(portal).call(callData);
        if (!success) {
            assembly ("memory-safe") {
                revert(add(revertData, 0x20), mload(revertData))
            }
        }
    }

    function _memberSet(uint256 a, uint256 b, uint256 c)
        internal
        returns (address[] memory roster)
    {
        roster = new address[](3);
        roster[0] = vm.addr(a);
        roster[1] = vm.addr(b);
        roster[2] = vm.addr(c);
    }

    function _peerSet() internal pure returns (address[] memory result) {
        result = new address[](9);
        for (uint256 i; i < result.length; ++i) {
            result[i] = address(uint160(0x9000 + i));
        }
    }

    function _copy(address[] memory source) internal pure returns (address[] memory result) {
        result = new address[](source.length);
        for (uint256 i; i < source.length; ++i) {
            result[i] = source[i];
        }
    }

    function _rosterHash(
        ZonePortal target,
        uint64 epoch,
        address[] memory roster,
        address[] memory peerSet
    )
        internal
        view
        returns (bytes32)
    {
        return keccak256(
            abi.encode(
                ROSTER_TAG,
                address(target),
                epoch,
                PROTOCOL_VERSION,
                uint8(2),
                FastProofMode.OperatorAttested,
                verifierCodeHash,
                keccak256(""),
                roster,
                peerSet
            )
        );
    }

    function _batch(string memory label) internal returns (FastBatch memory batch) {
        vm.roll(block.number + 1);
        uint64 tempoBlockNumber = uint64(block.number - 1);
        batch = FastBatch({
            tempoBlockNumber: tempoBlockNumber,
            blockTransition: BlockTransition({
                prevBlockHash: portal.blockHash(),
                nextBlockHash: keccak256(abi.encode(label, portal.zoneHeight() + 1))
            }),
            depositTransition: DepositQueueTransition({
                prevProcessedHash: bytes32(0),
                nextProcessedHash: bytes32(0),
                prevDepositNumber: portal.lastProcessedDepositNumber(),
                nextDepositNumber: portal.lastProcessedDepositNumber()
            }),
            tokenTransition: _currentTokenEnablementTransition(portal),
            withdrawalQueueHash: bytes32(0),
            verifierConfig: "",
            zoneHeight: portal.zoneHeight() + 1
        });
    }

    function _fastDigest(
        FastBatch memory batch,
        uint64 epoch,
        bytes32 rosterHash
    )
        internal
        view
        returns (bytes32)
    {
        return _fastDigestForPrefix(
            batch,
            epoch,
            rosterHash,
            portal.zoneHeight(),
            portal.blockHash(),
            portal.withdrawalBatchIndex()
        );
    }

    function _fastDigestForPrefix(
        FastBatch memory batch,
        uint64 epoch,
        bytes32 rosterHash,
        uint256 previousHeight,
        bytes32 previousBlockHash,
        uint64 previousWithdrawalBatchIndex
    )
        internal
        view
        returns (bytes32)
    {
        bytes32 structHash = keccak256(
            abi.encode(
                FAST_SETTLEMENT_TYPEHASH,
                portal.zoneId(),
                epoch,
                rosterHash,
                previousHeight,
                previousBlockHash,
                previousWithdrawalBatchIndex,
                batch.zoneHeight,
                previousWithdrawalBatchIndex + 1,
                portal.verifier(),
                batch.tempoBlockNumber,
                batch.tempoBlockNumber,
                getBlockHash(batch.tempoBlockNumber),
                keccak256(abi.encode(batch.blockTransition)),
                keccak256(abi.encode(batch.depositTransition)),
                keccak256(abi.encode(batch.tokenTransition)),
                batch.withdrawalQueueHash,
                keccak256(batch.verifierConfig)
            )
        );
        return _typedDigest(structHash);
    }

    function _legacyDigest(FastBatch memory batch) internal view returns (bytes32) {
        bytes32 structHash = keccak256(
            abi.encode(
                LEGACY_SETTLEMENT_TYPEHASH,
                portal.zoneId(),
                portal.sequencerSetVersion(),
                batch.zoneHeight,
                portal.withdrawalBatchIndex() + 1,
                portal.verifier(),
                batch.tempoBlockNumber,
                batch.tempoBlockNumber,
                getBlockHash(batch.tempoBlockNumber),
                keccak256(abi.encode(batch.blockTransition)),
                keccak256(abi.encode(batch.depositTransition)),
                keccak256(abi.encode(batch.tokenTransition)),
                batch.withdrawalQueueHash,
                keccak256(batch.verifierConfig)
            )
        );
        return _typedDigest(structHash);
    }

    function _typedDigest(bytes32 structHash) internal view returns (bytes32) {
        bytes32 domain = keccak256(
            abi.encode(
                EIP712_DOMAIN_TYPEHASH,
                keccak256("ZonePortal"),
                keccak256("1"),
                block.chainid,
                address(portal)
            )
        );
        return keccak256(abi.encodePacked("\x19\x01", domain, structHash));
    }

    function _sign(uint256 key, bytes32 digest) internal returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return abi.encodePacked(r, s, v);
    }

    function _pair(
        uint256 firstKey,
        uint256 secondKey,
        bytes32 firstDigest,
        bytes32 secondDigest
    )
        internal
        returns (bytes[] memory signatures)
    {
        signatures = new bytes[](2);
        signatures[0] = _sign(firstKey, firstDigest);
        signatures[1] = _sign(secondKey, secondDigest);
    }

    function _submit(FastBatch memory batch, bytes[] memory signatures) internal {
        portal.submitBatch(
            batch.tempoBlockNumber,
            0,
            batch.blockTransition,
            batch.depositTransition,
            batch.tokenTransition,
            batch.withdrawalQueueHash,
            batch.verifierConfig,
            "",
            batch.zoneHeight,
            signatures
        );
    }

    function _expectInvalidCertificate(FastBatch memory batch, bytes[] memory signatures) internal {
        bytes32 blockHashBefore = portal.blockHash();
        uint256 heightBefore = portal.zoneHeight();
        uint64 batchIndexBefore = portal.withdrawalBatchIndex();
        vm.prank(members[0]);
        vm.expectRevert(IZonePortal.InvalidQuorumCertificate.selector);
        _submit(batch, signatures);
        assertEq(portal.blockHash(), blockHashBefore);
        assertEq(portal.zoneHeight(), heightBefore);
        assertEq(portal.withdrawalBatchIndex(), batchIndexBefore);
    }

    function _settleWith(uint256 firstKey, uint256 secondKey, string memory label) internal {
        FastBatch memory batch = _batch(label);
        bytes32 digest = _fastDigest(batch, EPOCH, _rosterHash(portal, EPOCH, members, peers));
        vm.prank(vm.addr(firstKey));
        _submit(batch, _pair(firstKey, secondKey, digest, digest));
        assertEq(portal.blockHash(), batch.blockTransition.nextBlockHash);
        assertEq(portal.zoneHeight(), batch.zoneHeight);
        assertEq(portal.withdrawalBatchIndex(), 1);
    }

    function _barrierStatement(
        address peer,
        bytes32 unresolvedRoot,
        uint64 unresolvedCount
    )
        internal
        view
        returns (FastBarrierStatement memory statement)
    {
        statement = FastBarrierStatement({
            destinationPortal: address(portal),
            destinationEpoch: EPOCH,
            closureHash: keccak256("closure"),
            sourcePortal: peer,
            sourceEpoch: EPOCH,
            importedAnchorNumber: uint64(block.number),
            importedAnchorHash: keccak256("imported-anchor"),
            logTerm: 3,
            logIndex: 100,
            blockHeight: 99,
            blockHash: keccak256("source-block"),
            stateRoot: keccak256("source-state"),
            lockLogWatermark: 100,
            completeLockRoot: keccak256(abi.encode("complete-locks", peer)),
            unresolvedRoot: unresolvedRoot,
            unresolvedCount: unresolvedCount
        });
    }

    function _recordBarrier(address peer, bytes32 unresolvedRoot, uint64 unresolvedCount) internal {
        FastBarrierStatement memory statement = _barrierStatement(peer, unresolvedRoot, unresolvedCount);
        bytes32 digest = keccak256(abi.encode(BARRIER_TAG, block.chainid, statement));
        _factoryCall(
            abi.encodeCall(
                IZonePortal.recordFastPeerBarrier,
                (statement, _pair(MEMBER_A_KEY, MEMBER_B_KEY, digest, digest))
            )
        );
    }

    function _finalizeBarrier(address peer) internal {
        FastPeerBarrier memory barrier = portal.fastPeerBarrier(EPOCH, peer);
        FastBarrierResolution memory resolution = FastBarrierResolution({
            barrierHash: barrier.barrierHash,
            terminalRoot: keccak256(abi.encode("terminal", peer)),
            dispositionRoot: keccak256(abi.encode("dispositions", peer)),
            resolvedCount: barrier.unresolvedCount,
            remainingUnresolvedRoot: EMPTY_UNRESOLVED_ROOT,
            remainingUnresolvedCount: 0
        });
        bytes32 digest = keccak256(
            abi.encode(RESOLUTION_TAG, block.chainid, address(portal), EPOCH, peer, resolution)
        );
        _factoryCall(
            abi.encodeCall(
                IZonePortal.finalizeFastPeerBarrier,
                (EPOCH, peer, resolution, _pair(MEMBER_A_KEY, MEMBER_B_KEY, digest, digest))
            )
        );
    }

    function _recordAndFinalizeBarrier(
        uint256 index,
        bytes32 unresolvedRoot,
        uint64 unresolvedCount
    )
        internal
    {
        _recordBarrier(peers[index], unresolvedRoot, unresolvedCount);
        _finalizeBarrier(peers[index]);
    }

    function _finalizeAllBarriers() internal {
        for (uint256 i; i < peers.length; ++i) {
            _recordAndFinalizeBarrier(i, EMPTY_UNRESOLVED_ROOT, 0);
        }
    }

    function _recordFinalSettlement() internal {
        bytes32 barriersHash = keccak256("TEMPO_ZONE_FAST_BARRIERS_T14_V1");
        for (uint256 i; i < peers.length; ++i) {
            FastPeerBarrier memory barrier = portal.fastPeerBarrier(EPOCH, peers[i]);
            barriersHash = keccak256(
                abi.encode(barriersHash, peers[i], barrier.barrierHash, barrier.resolutionHash)
            );
        }
        FastEpochConfig memory config = portal.fastEpochConfig(EPOCH);
        bytes32 digest = keccak256(
            abi.encode(
                FINAL_SETTLEMENT_TAG,
                block.chainid,
                address(portal),
                EPOCH,
                config.rosterHash,
                config.closureHash,
                portal.zoneHeight(),
                portal.blockHash(),
                portal.withdrawalBatchIndex(),
                barriersHash
            )
        );
        _factoryCall(
            abi.encodeCall(
                IZonePortal.recordFastFinalSettlement,
                (
                    EPOCH,
                    portal.zoneHeight(),
                    portal.blockHash(),
                    portal.withdrawalBatchIndex(),
                    _pair(MEMBER_A_KEY, MEMBER_B_KEY, digest, digest)
                )
            )
        );
    }

    function _checkpointStatement(
        uint64 nextEpoch,
        address[] memory nextMembers
    )
        internal
        view
        returns (FastCheckpointStatement memory statement)
    {
        bytes32 nextRosterHash = _rosterHash(portal, nextEpoch, nextMembers, peers);
        FastEpochConfig memory config = portal.fastEpochConfig(EPOCH);
        statement = FastCheckpointStatement({
            portal: address(portal),
            oldEpoch: EPOCH,
            nextEpoch: nextEpoch,
            nextRosterHash: nextRosterHash,
            finalZoneHeight: config.finalSettlementHeight,
            finalBlockHash: config.finalSettlementBlockHash,
            finalWithdrawalBatchIndex: config.finalSettlementWithdrawalBatchIndex,
            finalSettlementHash: config.finalSettlementHash,
            checkpointLogTerm: 4,
            checkpointLogIndex: 101,
            checkpointHeight: config.finalSettlementHeight,
            checkpointBlockHash: config.finalSettlementBlockHash,
            checkpointStateRoot: keccak256("checkpoint-state")
        });
    }

    function _installCheckpoint(uint64 nextEpoch) internal returns (bytes32 checkpoint) {
        address[] memory nextMembers = _memberSet(0x101, 0x102, 0x103);
        FastCheckpointStatement memory statement = _checkpointStatement(nextEpoch, nextMembers);
        checkpoint = keccak256(abi.encode(CHECKPOINT_TAG, block.chainid, statement));
        _factoryCall(
            abi.encodeCall(
                IZonePortal.installFastCheckpoint,
                (
                    statement,
                    nextMembers,
                    _pair(0x101, 0x102, checkpoint, checkpoint)
                )
            )
        );
    }

    function _expectRejectedCheckpoint(
        FastCheckpointStatement memory statement,
        address[] memory nextMembers
    )
        internal
    {
        FastEpochConfig memory beforeConfig = portal.fastEpochConfig(EPOCH);
        bytes32 digest = keccak256(abi.encode(CHECKPOINT_TAG, block.chainid, statement));
        vm.expectRevert(IZonePortal.InvalidFastCertificate.selector);
        _factoryCall(
            abi.encodeCall(
                IZonePortal.installFastCheckpoint,
                (statement, nextMembers, _pair(0x101, 0x102, digest, digest))
            )
        );
        FastEpochConfig memory afterConfig = portal.fastEpochConfig(EPOCH);
        assertEq(
            keccak256(abi.encode(afterConfig)),
            keccak256(abi.encode(beforeConfig)),
            "invalid checkpoint mutated epoch state"
        );
    }

    function _expectNotDrainedRetirement(uint16 finalized, uint16 expected) internal {
        vm.expectRevert(
            abi.encodeWithSelector(
                IZonePortal.FastEpochNotDrained.selector, EPOCH, finalized, expected
            )
        );
        _factoryCall(abi.encodeCall(IZonePortal.retireFastEpoch, (EPOCH)));
    }

}
