// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title ITIP403Mirror
/// @notice Zone-side mirror of L1 TIP-403 registry
/// @dev Predeploy at 0x403c000000000000000000000000000000000001
///      Zone node provides L1 state values; prover validates against l1StateRoot
interface ITIP403Mirror {
    /// @notice Check if an address is authorized under a policy
    /// @param policyId 0 = always-reject, 1 = always-allow, 2+ = custom policy
    /// @param user The address to check
    function isAuthorized(uint64 policyId, address user) external view returns (bool);
}
