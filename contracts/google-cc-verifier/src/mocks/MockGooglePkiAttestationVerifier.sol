// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IGooglePkiAttestationVerifier} from "../interfaces/IGooglePkiAttestationVerifier.sol";

contract MockGooglePkiAttestationVerifier is IGooglePkiAttestationVerifier {
    Claims internal nextClaims;

    function setClaims(Claims calldata claims_) external {
        nextClaims = claims_;
    }

    function verifyAndExtract(Proof calldata) external view returns (Claims memory) {
        return nextClaims;
    }
}
