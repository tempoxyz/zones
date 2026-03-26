// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {GoogleConfidentialSpaceValidator} from "../src/GoogleConfidentialSpaceValidator.sol";
import {GooglePkiAttestationVerifier} from "../src/GooglePkiAttestationVerifier.sol";
import {IGooglePkiAttestationVerifier} from "../src/interfaces/IGooglePkiAttestationVerifier.sol";
import {TestGooglePkiFixture} from "./TestGooglePkiFixture.sol";

interface VmE2E {
    function warp(uint256 newTimestamp) external;
}

contract GoogleConfidentialSpaceValidatorE2ETest {
    VmE2E internal constant vm = VmE2E(address(uint160(uint256(keccak256("hevm cheat code")))));

    bytes32 internal constant IMAGE_DIGEST_HASH =
        keccak256(bytes("sha256:1111111111111111111111111111111111111111111111111111111111111111"));

    GooglePkiAttestationVerifier internal pkiVerifier;
    GoogleConfidentialSpaceValidator internal validator;

    function setUp() public {
        vm.warp(1_763_000_000);

        pkiVerifier = new GooglePkiAttestationVerifier(TestGooglePkiFixture.ROOT_DER);
        validator = new GoogleConfidentialSpaceValidator(
            pkiVerifier,
            GoogleConfidentialSpaceValidator.Policy({imageDigestHash: IMAGE_DIGEST_HASH, maxTokenAge: 300})
        );
    }

    function testVerifyAndExtractAcceptsValidGooglePkiProof() public view {
        IGooglePkiAttestationVerifier.Claims memory claims = pkiVerifier.verifyAndExtract(_proof());

        require(
            claims.issuerHash == keccak256(bytes("https://confidentialcomputing.googleapis.com")), "issuer mismatch"
        );
        require(claims.eatNonce == TestGooglePkiFixture.EAT_NONCE, "nonce mismatch");
        require(claims.issuedAt == 1_763_000_000, "iat mismatch");
        require(claims.notBefore == 1_762_999_990, "nbf mismatch");
        require(claims.expiresAt == 1_763_000_300, "exp mismatch");
        require(claims.hwModelHash == keccak256(bytes("GCP_INTEL_TDX")), "hwmodel mismatch");
        require(claims.debugStateHash == keccak256(bytes("disabled-since-boot")), "dbgstat mismatch");
        require(claims.imageDigestHash == IMAGE_DIGEST_HASH, "image digest mismatch");
    }

    function testVerifyAttestationAcceptsRealProof() public view {
        bool valid = validator.verifyAttestation(_proof(), TestGooglePkiFixture.EAT_NONCE);
        require(valid, "expected true");
    }

    function testVerifyAttestationRejectsTamperedJwsSignature() public view {
        IGooglePkiAttestationVerifier.Proof memory proof = _proof();
        proof.signature = _tamperedSignature();

        try validator.verifyAttestation(proof, TestGooglePkiFixture.EAT_NONCE) returns (bool) {
            revert("expected revert");
        } catch Error(string memory reason) {
            require(keccak256(bytes(reason)) == keccak256(bytes("jws sig invalid")), "unexpected revert");
        }
    }

    function _proof() internal pure returns (IGooglePkiAttestationVerifier.Proof memory proof) {
        proof.signingInput = TestGooglePkiFixture.SIGNING_INPUT;
        proof.signature = TestGooglePkiFixture.JWS_SIGNATURE;
        proof.leafCertificateDer = TestGooglePkiFixture.LEAF_DER;
        proof.intermediateCertificateDer = TestGooglePkiFixture.INTERMEDIATE_DER;
    }

    function _tamperedSignature() internal pure returns (bytes memory signature) {
        bytes memory original = TestGooglePkiFixture.JWS_SIGNATURE;
        signature = new bytes(original.length);
        for (uint256 i = 0; i < original.length; i++) {
            signature[i] = original[i];
        }
        signature[signature.length - 1] = bytes1(uint8(signature[signature.length - 1]) ^ 0x01);
    }
}
