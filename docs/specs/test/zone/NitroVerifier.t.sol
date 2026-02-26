// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BlockTransition, DepositQueueTransition } from "../../src/zone/IZone.sol";
import { NitroVerifier } from "../../src/zone/NitroVerifier.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { Vm } from "forge-std/Vm.sol";

/// @title NitroVerifierTest
/// @notice Tests for NitroVerifier TEE attestation-backed signature verification
contract NitroVerifierTest is BaseTest {

    NitroVerifier public verifier;

    Vm.Wallet internal attesterWallet;
    Vm.Wallet internal enclaveWallet;
    Vm.Wallet internal rogueWallet;

    address internal portal;
    address internal sequencer;

    bytes32 constant MEASUREMENT_HASH = keccak256("PCR0||PCR1||PCR2");
    uint64 constant EXPIRES_AT = 1_000_000;

    // Batch params
    uint64 constant TEMPO_BLOCK_NUMBER = 100;
    uint64 constant ANCHOR_BLOCK_NUMBER = 99;
    bytes32 constant ANCHOR_BLOCK_HASH = keccak256("anchor");
    uint64 constant EXPECTED_WITHDRAWAL_BATCH_INDEX = 1;
    bytes32 constant PREV_BLOCK_HASH = keccak256("prev");
    bytes32 constant NEXT_BLOCK_HASH = keccak256("next");
    bytes32 constant PREV_PROCESSED_HASH = keccak256("prevDeposit");
    bytes32 constant NEXT_PROCESSED_HASH = keccak256("nextDeposit");
    bytes32 constant WITHDRAWAL_QUEUE_HASH = keccak256("withdrawal");

    function setUp() public override {
        super.setUp();

        attesterWallet = vm.createWallet("attester");
        enclaveWallet = vm.createWallet("enclave");
        rogueWallet = vm.createWallet("rogue");

        portal = address(0x1234);
        sequencer = address(0x5678);

        verifier = new NitroVerifier(attesterWallet.addr);

        // Set block.timestamp below expiry
        vm.warp(500_000);
    }

    /*//////////////////////////////////////////////////////////////
                      REGISTER ENCLAVE KEY TESTS
    //////////////////////////////////////////////////////////////*/

    function test_registerEnclaveKey_validAttesterSig() public {
        bytes memory sig = _signRegistration(
            attesterWallet, portal, sequencer, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0
        );

        verifier.registerEnclaveKey(
            portal, sequencer, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0, sig
        );

        (address key, bytes32 mHash, uint64 exp, address seq) = verifier.registrations(portal);
        assertEq(key, enclaveWallet.addr);
        assertEq(mHash, MEASUREMENT_HASH);
        assertEq(exp, EXPIRES_AT);
        assertEq(seq, sequencer);
        assertEq(verifier.registrationNonce(portal), 1);
    }

    function test_registerEnclaveKey_emitsEvent() public {
        bytes memory sig = _signRegistration(
            attesterWallet, portal, sequencer, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0
        );

        vm.expectEmit(true, true, false, true);
        emit NitroVerifier.EnclaveKeyRegistered(
            portal, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0
        );

        verifier.registerEnclaveKey(
            portal, sequencer, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0, sig
        );
    }

    function test_registerEnclaveKey_invalidAttesterSig_reverts() public {
        bytes memory sig = _signRegistration(
            rogueWallet, portal, sequencer, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0
        );

        vm.expectRevert(NitroVerifier.InvalidAttesterSignature.selector);
        verifier.registerEnclaveKey(
            portal, sequencer, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0, sig
        );
    }

    function test_registerEnclaveKey_replayPrevented() public {
        // First registration at nonce 0
        bytes memory sig0 = _signRegistration(
            attesterWallet, portal, sequencer, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0
        );
        verifier.registerEnclaveKey(
            portal, sequencer, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0, sig0
        );

        // Replaying the same nonce 0 signature should revert
        vm.expectRevert(NitroVerifier.InvalidNonce.selector);
        verifier.registerEnclaveKey(
            portal, sequencer, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0, sig0
        );
    }

    function test_registerEnclaveKey_keyRotation() public {
        // Register first key at nonce 0
        _registerEnclave();
        assertEq(verifier.registrationNonce(portal), 1);

        // Register a new key at nonce 1
        Vm.Wallet memory newEnclaveWallet = vm.createWallet("newEnclave");
        bytes memory sig1 = _signRegistration(
            attesterWallet, portal, sequencer, newEnclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 1
        );
        verifier.registerEnclaveKey(
            portal, sequencer, newEnclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 1, sig1
        );

        (address key,,,) = verifier.registrations(portal);
        assertEq(key, newEnclaveWallet.addr);
        assertEq(verifier.registrationNonce(portal), 2);
    }

    /*//////////////////////////////////////////////////////////////
                         VERIFY TESTS
    //////////////////////////////////////////////////////////////*/

    function test_verify_validEnclaveSig() public {
        _registerEnclave();

        bytes memory enclaveSig = _signBatch(enclaveWallet);
        bytes memory proof = abi.encode(enclaveSig);

        vm.prank(portal);
        bool result = verifier.verify(
            TEMPO_BLOCK_NUMBER,
            ANCHOR_BLOCK_NUMBER,
            ANCHOR_BLOCK_HASH,
            EXPECTED_WITHDRAWAL_BATCH_INDEX,
            sequencer,
            BlockTransition(PREV_BLOCK_HASH, NEXT_BLOCK_HASH),
            DepositQueueTransition(PREV_PROCESSED_HASH, NEXT_PROCESSED_HASH),
            WITHDRAWAL_QUEUE_HASH,
            "",
            proof
        );

        assertTrue(result);
    }

    function test_verify_invalidEnclaveSig_reverts() public {
        _registerEnclave();

        bytes memory rogueSig = _signBatch(rogueWallet);
        bytes memory proof = abi.encode(rogueSig);

        vm.prank(portal);
        vm.expectRevert(NitroVerifier.InvalidEnclaveSignature.selector);
        verifier.verify(
            TEMPO_BLOCK_NUMBER,
            ANCHOR_BLOCK_NUMBER,
            ANCHOR_BLOCK_HASH,
            EXPECTED_WITHDRAWAL_BATCH_INDEX,
            sequencer,
            BlockTransition(PREV_BLOCK_HASH, NEXT_BLOCK_HASH),
            DepositQueueTransition(PREV_PROCESSED_HASH, NEXT_PROCESSED_HASH),
            WITHDRAWAL_QUEUE_HASH,
            "",
            proof
        );
    }

    function test_verify_expiredKey_reverts() public {
        _registerEnclave();

        // Warp past expiry
        vm.warp(EXPIRES_AT + 1);

        bytes memory enclaveSig = _signBatch(enclaveWallet);
        bytes memory proof = abi.encode(enclaveSig);

        vm.prank(portal);
        vm.expectRevert(NitroVerifier.EnclaveKeyExpired.selector);
        verifier.verify(
            TEMPO_BLOCK_NUMBER,
            ANCHOR_BLOCK_NUMBER,
            ANCHOR_BLOCK_HASH,
            EXPECTED_WITHDRAWAL_BATCH_INDEX,
            sequencer,
            BlockTransition(PREV_BLOCK_HASH, NEXT_BLOCK_HASH),
            DepositQueueTransition(PREV_PROCESSED_HASH, NEXT_PROCESSED_HASH),
            WITHDRAWAL_QUEUE_HASH,
            "",
            proof
        );
    }

    function test_verify_noRegistration_reverts() public {
        bytes memory enclaveSig = _signBatch(enclaveWallet);
        bytes memory proof = abi.encode(enclaveSig);

        vm.prank(portal);
        vm.expectRevert(NitroVerifier.EnclaveKeyNotRegistered.selector);
        verifier.verify(
            TEMPO_BLOCK_NUMBER,
            ANCHOR_BLOCK_NUMBER,
            ANCHOR_BLOCK_HASH,
            EXPECTED_WITHDRAWAL_BATCH_INDEX,
            sequencer,
            BlockTransition(PREV_BLOCK_HASH, NEXT_BLOCK_HASH),
            DepositQueueTransition(PREV_PROCESSED_HASH, NEXT_PROCESSED_HASH),
            WITHDRAWAL_QUEUE_HASH,
            "",
            proof
        );
    }

    function test_computeBatchDigest_deterministic() public view {
        bytes32 digest1 = verifier.computeBatchDigest(
            TEMPO_BLOCK_NUMBER,
            ANCHOR_BLOCK_NUMBER,
            ANCHOR_BLOCK_HASH,
            EXPECTED_WITHDRAWAL_BATCH_INDEX,
            sequencer,
            BlockTransition(PREV_BLOCK_HASH, NEXT_BLOCK_HASH),
            DepositQueueTransition(PREV_PROCESSED_HASH, NEXT_PROCESSED_HASH),
            WITHDRAWAL_QUEUE_HASH,
            portal
        );

        bytes32 digest2 = verifier.computeBatchDigest(
            TEMPO_BLOCK_NUMBER,
            ANCHOR_BLOCK_NUMBER,
            ANCHOR_BLOCK_HASH,
            EXPECTED_WITHDRAWAL_BATCH_INDEX,
            sequencer,
            BlockTransition(PREV_BLOCK_HASH, NEXT_BLOCK_HASH),
            DepositQueueTransition(PREV_PROCESSED_HASH, NEXT_PROCESSED_HASH),
            WITHDRAWAL_QUEUE_HASH,
            portal
        );

        assertEq(digest1, digest2);
        assertTrue(digest1 != bytes32(0));
    }

    /*//////////////////////////////////////////////////////////////
                              HELPERS
    //////////////////////////////////////////////////////////////*/

    function _registerEnclave() internal {
        bytes memory sig = _signRegistration(
            attesterWallet, portal, sequencer, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0
        );
        verifier.registerEnclaveKey(
            portal, sequencer, enclaveWallet.addr, MEASUREMENT_HASH, EXPIRES_AT, 0, sig
        );
    }

    function _signRegistration(
        Vm.Wallet memory signer,
        address _portal,
        address _sequencer,
        address _enclaveKey,
        bytes32 _measurementHash,
        uint64 _expiresAt,
        uint64 _nonce
    )
        internal
        returns (bytes memory)
    {
        bytes32 digest = keccak256(
            abi.encode(
                "NitroVerifier.RegisterEnclaveKey",
                block.chainid,
                address(verifier),
                _portal,
                _sequencer,
                _enclaveKey,
                _measurementHash,
                _expiresAt,
                _nonce
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(signer, digest);
        return abi.encodePacked(r, s, v);
    }

    function _signBatch(Vm.Wallet memory signer) internal returns (bytes memory) {
        bytes32 digest = verifier.computeBatchDigest(
            TEMPO_BLOCK_NUMBER,
            ANCHOR_BLOCK_NUMBER,
            ANCHOR_BLOCK_HASH,
            EXPECTED_WITHDRAWAL_BATCH_INDEX,
            sequencer,
            BlockTransition(PREV_BLOCK_HASH, NEXT_BLOCK_HASH),
            DepositQueueTransition(PREV_PROCESSED_HASH, NEXT_PROCESSED_HASH),
            WITHDRAWAL_QUEUE_HASH,
            portal
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(signer, digest);
        return abi.encodePacked(r, s, v);
    }

}
