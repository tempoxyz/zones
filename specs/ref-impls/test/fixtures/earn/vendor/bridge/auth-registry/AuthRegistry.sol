// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IAuthRegistry } from "./IAuthRegistry.sol";

/**
 * @title AuthRegistry
 * @notice A registry for managing whitelist and blacklist authorization policies, and checking
 *         authorization for accounts according to those policies.
 */
contract AuthRegistry is IAuthRegistry {
    uint64 public constant FIRST_USER_POLICY = 2;

    uint64 public policyIdCounter = 2; // Skip special policies (documented in isAuthorized).

    mapping(uint64 => PolicyData) public policyData;

    /*//////////////////////////////////////////////////////////////
                      POLICY TYPE-SPECIFIC STORAGE
    //////////////////////////////////////////////////////////////*/

    mapping(uint64 => mapping(address => bool)) internal policySet;

    /*//////////////////////////////////////////////////////////////
                      GENERAL POLICY ADMINISTRATION
    //////////////////////////////////////////////////////////////*/

    function createPolicy(address admin, PolicyType policyType) public returns (uint64 newPolicyId) {
        return _createPolicy(admin, policyType, 0, false);
    }

    function createPolicy(address admin, PolicyType policyType, uint64 parentPolicyId)
        public
        returns (uint64 newPolicyId)
    {
        return _createPolicy(admin, policyType, parentPolicyId, true);
    }

    function createPolicy(address admin, PolicyType policyType, address[] memory accounts)
        public
        returns (uint64 newPolicyId)
    {
        newPolicyId = _createPolicy(admin, policyType, 0, false);

        // Set the initial policy set.
        for (uint256 i = 0; i < accounts.length; i++) {
            policySet[newPolicyId][accounts[i]] = true;

            if (policyType == PolicyType.WHITELIST) {
                emit WhitelistUpdated(newPolicyId, msg.sender, accounts[i], true);
            } else {
                emit BlacklistUpdated(newPolicyId, msg.sender, accounts[i], true);
            }
        }
    }

    function createPolicy(address admin, PolicyType policyType, uint64 parentPolicyId, address[] memory accounts)
        public
        returns (uint64 newPolicyId)
    {
        newPolicyId = _createPolicy(admin, policyType, parentPolicyId, true);

        // Set the initial policy set.
        for (uint256 i = 0; i < accounts.length; i++) {
            policySet[newPolicyId][accounts[i]] = true;

            if (policyType == PolicyType.WHITELIST) {
                emit WhitelistUpdated(newPolicyId, msg.sender, accounts[i], true);
            } else {
                emit BlacklistUpdated(newPolicyId, msg.sender, accounts[i], true);
            }
        }
    }

    function setPolicyAdmin(uint64 policyId, address admin) external {
        require(policyData[policyId].admin == msg.sender, Unauthorized());

        policyData[policyId].admin = admin;

        emit PolicyAdminUpdated(policyId, msg.sender, admin);
    }

    function modifyParentPolicy(uint64 policyId, uint64 parentPolicyId, bool setting) external {
        require(policyData[policyId].admin == msg.sender, Unauthorized());

        if (setting) {
            _validateParentPolicy(policyData[policyId].policyType, policyId, parentPolicyId);
            policyData[policyId].parentPolicyId = parentPolicyId;
            policyData[policyId].parentPolicyIdIsSet = true;
        } else {
            policyData[policyId].parentPolicyId = 0;
            policyData[policyId].parentPolicyIdIsSet = false;
        }

        emit ParentPolicyUpdated(policyId, msg.sender, parentPolicyId, setting);
    }

    /*//////////////////////////////////////////////////////////////
                   POLICY TYPE-SPECIFIC ADMINISTRATION
    //////////////////////////////////////////////////////////////*/

    function modifyPolicyWhitelist(uint64 policyId, address account, bool allowed) external {
        _modifyPolicyWhitelist(policyId, account, allowed);
    }

    function modifyPolicyBlacklist(uint64 policyId, address account, bool restricted) external {
        _modifyPolicyBlacklist(policyId, account, restricted);
    }

    function batchModifyPolicyWhitelist(uint64 policyId, address[] calldata accounts, bool allowed) external {
        for (uint256 i = 0; i < accounts.length; i++) {
            _modifyPolicyWhitelist(policyId, accounts[i], allowed);
        }
    }

    function batchModifyPolicyBlacklist(uint64 policyId, address[] calldata accounts, bool restricted) external {
        for (uint256 i = 0; i < accounts.length; i++) {
            _modifyPolicyBlacklist(policyId, accounts[i], restricted);
        }
    }

    /*//////////////////////////////////////////////////////////////
                        GENERAL POLICY QUERYING
    //////////////////////////////////////////////////////////////*/

    function isAuthorized(uint64 policyId, address user) public view override returns (bool) {
        PolicyData memory data = policyData[policyId];

        if (policyId < FIRST_USER_POLICY) {
            // policyId == 0 is the "always-reject" policy.
            // policyId == 1 is the "always-allow" policy.
            return policyId == 1;
        }

        bool policySetForUser = policySet[policyId][user];

        if (policySetForUser) {
            return data.policyType == PolicyType.WHITELIST;
        }

        if (data.parentPolicyIdIsSet) {
            return isAuthorized(data.parentPolicyId, user);
        }

        return data.policyType == PolicyType.BLACKLIST;
    }

    /*//////////////////////////////////////////////////////////////
                        Internal Functions
    //////////////////////////////////////////////////////////////*/

    function _createPolicy(address admin, PolicyType policyType, uint64 parentPolicyId, bool shouldSetParentPolicyId)
        internal
        returns (uint64 newPolicyId)
    {
        newPolicyId = policyIdCounter++;
        parentPolicyId = shouldSetParentPolicyId ? parentPolicyId : 0;

        policyData[newPolicyId] = PolicyData({
            policyType: policyType,
            admin: admin,
            parentPolicyId: parentPolicyId,
            parentPolicyIdIsSet: shouldSetParentPolicyId
        });

        emit PolicyCreated(newPolicyId, msg.sender, policyType);
        emit PolicyAdminUpdated(newPolicyId, msg.sender, admin);

        if (shouldSetParentPolicyId) {
            _validateParentPolicy(policyType, newPolicyId, parentPolicyId);
            emit ParentPolicyUpdated(newPolicyId, msg.sender, parentPolicyId, true);
        }
    }

    function _modifyPolicyWhitelist(uint64 policyId, address account, bool allowed) internal {
        PolicyData memory data = policyData[policyId];

        require(data.admin == msg.sender, Unauthorized());
        require(data.policyType == PolicyType.WHITELIST, IncompatiblePolicyType());

        policySet[policyId][account] = allowed;

        emit WhitelistUpdated(policyId, msg.sender, account, allowed);
    }

    function _modifyPolicyBlacklist(uint64 policyId, address account, bool restricted) internal {
        PolicyData memory data = policyData[policyId];

        require(data.admin == msg.sender, Unauthorized());
        require(data.policyType == PolicyType.BLACKLIST, IncompatiblePolicyType());

        policySet[policyId][account] = restricted;

        emit BlacklistUpdated(policyId, msg.sender, account, restricted);
    }

    function _validateParentPolicy(PolicyType policyType, uint64 policyId, uint64 parentPolicyId) internal view {
        require(parentPolicyId >= 2, InvalidParentPolicyId());

        require(parentPolicyId < policyIdCounter && parentPolicyId != policyId, InvalidParentPolicyId());
        require(policyData[parentPolicyId].policyType == policyType, InvalidParentPolicyId());
    }
}
