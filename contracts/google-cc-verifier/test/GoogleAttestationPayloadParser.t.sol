// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IGooglePkiAttestationVerifier} from "../src/interfaces/IGooglePkiAttestationVerifier.sol";
import {GoogleAttestationPayloadParser} from "../src/libraries/GoogleAttestationPayloadParser.sol";

contract GoogleAttestationPayloadParserHarness {
    function parseRequiredClaims(bytes calldata payloadJson)
        external
        pure
        returns (IGooglePkiAttestationVerifier.Claims memory)
    {
        return GoogleAttestationPayloadParser.parseRequiredClaims(payloadJson);
    }
}

contract GoogleAttestationPayloadParserTest {
    bytes32 internal constant NONCE = hex"1111111111111111111111111111111111111111111111111111111111111111";
    bytes32 internal constant ARRAY_NONCE = hex"2222222222222222222222222222222222222222222222222222222222222222";

    GoogleAttestationPayloadParserHarness internal harness;

    function setUp() public {
        harness = new GoogleAttestationPayloadParserHarness();
    }

    function testParseRequiredClaimsExtractsPinnedFields() public view {
        bytes memory payloadJson = abi.encodePacked(
            "{",
            '"iss":"https://confidentialcomputing.googleapis.com",',
            '"eat_nonce":"0x1111111111111111111111111111111111111111111111111111111111111111",',
            '"iat":1763000000,',
            '"nbf":1762999990,',
            '"exp":1763000300,',
            '"dbgstat":"disabled-since-boot",',
            '"hwmodel":"GCP_INTEL_TDX",',
            '"submods":{"container":{"image_digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111"}}',
            "}"
        );

        IGooglePkiAttestationVerifier.Claims memory claims = harness.parseRequiredClaims(payloadJson);

        require(
            claims.issuerHash == keccak256(bytes("https://confidentialcomputing.googleapis.com")),
            "issuer hash mismatch"
        );
        require(claims.eatNonce == NONCE, "nonce mismatch");
        require(claims.issuedAt == 1763000000, "iat mismatch");
        require(claims.notBefore == 1762999990, "nbf mismatch");
        require(claims.expiresAt == 1763000300, "exp mismatch");
        require(claims.hwModelHash == keccak256(bytes("GCP_INTEL_TDX")), "hwmodel hash mismatch");
        require(claims.debugStateHash == keccak256(bytes("disabled-since-boot")), "dbgstat hash mismatch");
        require(
            claims.imageDigestHash
                == keccak256(bytes("sha256:1111111111111111111111111111111111111111111111111111111111111111")),
            "image digest mismatch"
        );
    }

    function testParseRequiredClaimsAcceptsSingletonNonceArray() public view {
        bytes memory payloadJson = abi.encodePacked(
            "{",
            '"iss":"https://confidentialcomputing.googleapis.com",',
            '"eat_nonce":["2222222222222222222222222222222222222222222222222222222222222222"],',
            '"iat":1763000000,',
            '"nbf":1762999990,',
            '"exp":1763000300,',
            '"dbgstat":"disabled-since-boot",',
            '"hwmodel":"GCP_INTEL_TDX",',
            '"submods":{"container":{"image_digest":"sha256:abc"}}',
            "}"
        );

        IGooglePkiAttestationVerifier.Claims memory claims = harness.parseRequiredClaims(payloadJson);
        require(claims.eatNonce == ARRAY_NONCE, "nonce array mismatch");
    }

    function testParseRequiredClaimsRejectsDuplicateClaim() public view {
        bytes memory payloadJson = abi.encodePacked(
            "{",
            '"iss":"https://confidentialcomputing.googleapis.com",',
            '"iss":"https://example.com",',
            '"eat_nonce":"0x1111111111111111111111111111111111111111111111111111111111111111",',
            '"iat":1763000000,',
            '"nbf":1762999990,',
            '"exp":1763000300,',
            '"dbgstat":"disabled-since-boot",',
            '"hwmodel":"GCP_INTEL_TDX",',
            '"submods":{"container":{"image_digest":"sha256:abc"}}',
            "}"
        );

        try harness.parseRequiredClaims(payloadJson) returns (IGooglePkiAttestationVerifier.Claims memory) {
            revert("expected revert");
        } catch Error(string memory reason) {
            require(keccak256(bytes(reason)) == keccak256(bytes("duplicate claim")), "unexpected revert");
        }
    }
}
