// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IGooglePkiAttestationVerifier {
    struct Proof {
        // Exact JWS signing input: base64url(header) || "." || base64url(payload).
        bytes signingInput;
        // Raw JWS signature bytes.
        bytes signature;
        bytes leafCertificateDer;
        bytes intermediateCertificateDer;
    }

    struct Claims {
        bytes32 issuerHash;
        bytes32 eatNonce;
        uint64 issuedAt;
        uint64 notBefore;
        uint64 expiresAt;
        bytes32 hwModelHash;
        bytes32 debugStateHash;
        bytes32 imageDigestHash;
    }

    // A concrete verifier is expected to authenticate the PKI/JWS proof and extract only the
    // Google Confidential Space claims required by the current onchain policy.
    function verifyAndExtract(Proof calldata proof) external view returns (Claims memory);
}
