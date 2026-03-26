// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {GoogleConfidentialSpaceValidator} from "../src/GoogleConfidentialSpaceValidator.sol";
import {IGooglePkiAttestationVerifier} from "../src/interfaces/IGooglePkiAttestationVerifier.sol";
import {MockGooglePkiAttestationVerifier} from "../src/mocks/MockGooglePkiAttestationVerifier.sol";

interface Vm {
    function warp(uint256 newTimestamp) external;
}

contract GoogleConfidentialSpaceValidatorTest {
    Vm internal constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    bytes32 internal constant IMAGE_DIGEST_HASH =
        keccak256(bytes("sha256:1111111111111111111111111111111111111111111111111111111111111111"));
    bytes32 internal constant NONCE = hex"1111111111111111111111111111111111111111111111111111111111111111";

    MockGooglePkiAttestationVerifier internal verifier;
    GoogleConfidentialSpaceValidator internal validator;

    function setUp() public {
        vm.warp(1_763_000_000);

        verifier = new MockGooglePkiAttestationVerifier();
        validator = new GoogleConfidentialSpaceValidator(
            verifier,
            GoogleConfidentialSpaceValidator.Policy({imageDigestHash: IMAGE_DIGEST_HASH, maxTokenAge: 300})
        );
    }

    function testVerifyAttestationAcceptsMatchingAttestation() public {
        verifier.setClaims(_claims(NONCE, IMAGE_DIGEST_HASH));

        bool valid = validator.verifyAttestation(_emptyProof(), NONCE);
        require(valid, "expected true");
    }

    function testVerifyAttestationRejectsNonceMismatch() public {
        verifier.setClaims(_claims(bytes32(uint256(1234)), IMAGE_DIGEST_HASH));

        try validator.verifyAttestation(_emptyProof(), NONCE) returns (bool) {
            revert("expected revert");
        } catch Error(string memory reason) {
            require(keccak256(bytes(reason)) == keccak256(bytes("nonce mismatch")), "unexpected revert");
        }
    }

    function testVerifyAttestationRejectsImageMismatch() public {
        verifier.setClaims(_claims(NONCE, keccak256(bytes("sha256:bad"))));

        try validator.verifyAttestation(_emptyProof(), NONCE) returns (bool) {
            revert("expected revert");
        } catch Error(string memory reason) {
            require(keccak256(bytes(reason)) == keccak256(bytes("image digest mismatch")), "unexpected revert");
        }
    }

    function _claims(bytes32 nonce, bytes32 imageDigestHash)
        internal
        view
        returns (IGooglePkiAttestationVerifier.Claims memory)
    {
        return IGooglePkiAttestationVerifier.Claims({
            issuerHash: keccak256(bytes("https://confidentialcomputing.googleapis.com")),
            eatNonce: nonce,
            issuedAt: uint64(block.timestamp),
            notBefore: uint64(block.timestamp - 1),
            expiresAt: uint64(block.timestamp + 300),
            hwModelHash: keccak256(bytes("GCP_INTEL_TDX")),
            debugStateHash: keccak256(bytes("disabled-since-boot")),
            imageDigestHash: imageDigestHash
        });
    }

    function _emptyProof() internal pure returns (IGooglePkiAttestationVerifier.Proof memory) {
        return IGooglePkiAttestationVerifier.Proof({
            signingInput: "", signature: "", leafCertificateDer: "", intermediateCertificateDer: ""
        });
    }
}
