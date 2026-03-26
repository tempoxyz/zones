// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IGooglePkiAttestationVerifier} from "./interfaces/IGooglePkiAttestationVerifier.sol";

contract GoogleConfidentialSpaceValidator {
    bytes32 public constant GOOGLE_ISSUER_HASH = keccak256(bytes("https://confidentialcomputing.googleapis.com"));
    bytes32 public constant GCP_INTEL_TDX_HASH = keccak256(bytes("GCP_INTEL_TDX"));
    bytes32 public constant PRODUCTION_DBGSTAT_HASH = keccak256(bytes("disabled-since-boot"));

    struct Policy {
        bytes32 imageDigestHash;
        uint64 maxTokenAge;
    }

    IGooglePkiAttestationVerifier public immutable attestationVerifier;
    bytes32 public immutable expectedImageDigestHash;
    uint64 public immutable maxTokenAge;

    constructor(IGooglePkiAttestationVerifier attestationVerifier_, Policy memory policy_) {
        require(address(attestationVerifier_) != address(0), "verifier required");

        attestationVerifier = attestationVerifier_;
        expectedImageDigestHash = policy_.imageDigestHash;
        maxTokenAge = policy_.maxTokenAge;
    }

    function verifyAttestation(IGooglePkiAttestationVerifier.Proof calldata proof, bytes32 expectedEatNonce)
        external
        view
        returns (bool)
    {
        IGooglePkiAttestationVerifier.Claims memory claims = attestationVerifier.verifyAndExtract(proof);
        _validateClaims(claims, expectedEatNonce);
        return true;
    }

    function _validateClaims(IGooglePkiAttestationVerifier.Claims memory claims, bytes32 expectedEatNonce)
        internal
        view
    {
        require(claims.issuerHash == GOOGLE_ISSUER_HASH, "issuer mismatch");
        require(claims.eatNonce == expectedEatNonce, "nonce mismatch");
        require(claims.hwModelHash == GCP_INTEL_TDX_HASH, "hwmodel mismatch");
        require(claims.debugStateHash == PRODUCTION_DBGSTAT_HASH, "dbgstat mismatch");
        require(claims.imageDigestHash == expectedImageDigestHash, "image digest mismatch");

        require(claims.notBefore <= block.timestamp, "token not active");
        require(claims.expiresAt >= block.timestamp, "token expired");
        require(claims.issuedAt <= block.timestamp, "issued in future");

        if (maxTokenAge != 0) {
            require(block.timestamp - claims.issuedAt <= maxTokenAge, "token too old");
        }
    }
}
