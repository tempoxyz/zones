// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.12;

interface IAuthRegistry {
    /*//////////////////////////////////////////////////////////////
                         GENERAL POLICY STORAGE
    //////////////////////////////////////////////////////////////*/

    enum PolicyType {
        WHITELIST,
        BLACKLIST
    }

    struct PolicyData {
        PolicyType policyType;
        address admin;
        uint64 parentPolicyId;
        bool parentPolicyIdIsSet;
    }

    /*//////////////////////////////////////////////////////////////
                                 ERRORS
    //////////////////////////////////////////////////////////////*/

    error Unauthorized();
    error IncompatiblePolicyType();
    error PolicyDoesNotExist();
    error ArrayLengthMismatch();
    error InvalidParentPolicyId();

    /*//////////////////////////////////////////////////////////////
                                 EVENTS
    //////////////////////////////////////////////////////////////*/

    event PolicyAdminUpdated(uint64 indexed policyId, address indexed updater, address indexed admin);
    event PolicyCreated(uint64 indexed policyId, address indexed updater, PolicyType policyType);
    event WhitelistUpdated(uint64 indexed policyId, address indexed updater, address indexed account, bool allowed);
    event BlacklistUpdated(uint64 indexed policyId, address indexed updater, address indexed account, bool restricted);
    event ParentPolicyUpdated(
        uint64 indexed policyId, address indexed updater, uint64 indexed parentPolicyId, bool set
    );

    /*//////////////////////////////////////////////////////////////
                        GENERAL POLICY QUERYING
    //////////////////////////////////////////////////////////////*/

    function isAuthorized(uint64 policyId, address user) external view returns (bool);
}
