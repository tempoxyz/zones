// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITIP403Registry } from "tempo-std/interfaces/ITIP403Registry.sol";

/// @notice Permissive mock of the TIP-403 transfer-policy registry used by
///         zone-side test suites that don't load the real zone genesis. Every
///         authorization query returns `true` so `_requestWithdrawal` and other
///         zone-side checks pass through unimpeded.
contract MockTIP403Registry is ITIP403Registry {

    function policyIdCounter() external pure returns (uint64) {
        return 0;
    }

    function policyExists(uint64) external pure returns (bool) {
        return true;
    }

    function policyData(uint64) external pure returns (PolicyType, address) {
        return (PolicyType.WHITELIST, address(0));
    }

    function createPolicy(address, PolicyType) external pure returns (uint64) {
        return 0;
    }

    function createPolicyWithAccounts(
        address,
        PolicyType,
        address[] calldata
    )
        external
        pure
        returns (uint64)
    {
        return 0;
    }

    function setPolicyAdmin(uint64, address) external pure { }

    function modifyPolicyWhitelist(uint64, address, bool) external pure { }

    function modifyPolicyBlacklist(uint64, address, bool) external pure { }

    function isAuthorized(uint64, address) external pure returns (bool) {
        return true;
    }

    function createCompoundPolicy(uint64, uint64, uint64) external pure returns (uint64) {
        return 0;
    }

    function isAuthorizedSender(uint64, address) external pure returns (bool) {
        return true;
    }

    function isAuthorizedRecipient(uint64, address) external pure returns (bool) {
        return true;
    }

    function isAuthorizedMintRecipient(uint64, address) external pure returns (bool) {
        return true;
    }

    function compoundPolicyData(uint64) external pure returns (uint64, uint64, uint64) {
        return (0, 0, 0);
    }

}
