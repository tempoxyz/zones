// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IVerifier } from "../../src/runtime/interfaces/IZone.sol";
import { ZonePortal } from "../../src/runtime/tempo/ZonePortal.sol";

/// @dev Test-only shared portal runtime. Retains portal state and certificate validation,
/// but routes proof verification to an ordinary contract instead of the Nitro precompile.
contract MockVerifierZonePortal is ZonePortal {

    function _proofVerifier() internal pure override returns (IVerifier) {
        return IVerifier(address(0xBEEF));
    }

}
