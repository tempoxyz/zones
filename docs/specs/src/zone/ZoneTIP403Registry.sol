// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITIP403Registry } from "../interfaces/ITIP403Registry.sol";

/// @title ZoneTIP403Registry
/// @notice Read-only TIP-403 policy registry for zones that reads state from Tempo
/// @dev This contract is deployed at the SAME ADDRESS as the Tempo TIP403Registry.
///      It provides read-only access to Tempo's policy state via the TempoState precompile.
///      Zone-side TIP-20 contracts call isAuthorized() to enforce Tempo TIP-403 policies.
///
///      IMPORTANT: Transactions calling this contract MUST include a TempoStateDeclaration
///      (tx type 0x7A) that declares the policy storage slots they will read.
///      Transactions without proper declarations are invalid.
///
///      Storage layout matches Tempo's TIP403Registry:
///      - slot 0: policyIdCounter (uint64)
///      - mapping(_policyData): keccak256(policyId, 1) → PolicyData
///      - mapping(policySet): keccak256(account, keccak256(policyId, 2)) → bool
contract ZoneTIP403Registry {
    /// @notice The TempoState predeploy for reading Tempo storage
    address public constant TEMPO_STATE = 0x1c00000000000000000000000000000000000000;

    /// @notice The address of the TIP403Registry on Tempo (same as this contract's address)
    /// @dev This contract MUST be deployed at the same address as the Tempo registry
    address public immutable TEMPO_REGISTRY;

    /// @notice Storage slot constants matching Tempo's TIP403Registry layout
    uint256 private constant SLOT_POLICY_ID_COUNTER = 0;
    uint256 private constant SLOT_POLICY_DATA_MAPPING = 1;
    uint256 private constant SLOT_POLICY_SET_MAPPING = 2;

    /// @notice Policy types (must match Tempo's ITIP403Registry.PolicyType)
    enum PolicyType {
        WHITELIST,
        BLACKLIST
    }

    error PolicyNotFound();

    constructor() {
        // This contract must be deployed at the same address as Tempo's TIP403Registry
        TEMPO_REGISTRY = address(this);
    }

    /*//////////////////////////////////////////////////////////////
                            READ-ONLY QUERIES
    //////////////////////////////////////////////////////////////*/

    /// @notice Returns the current policy ID counter from Tempo
    /// @dev Reads slot 0 from Tempo's TIP403Registry
    function policyIdCounter() public view returns (uint64) {
        bytes32 value = _readTempoSlot(bytes32(SLOT_POLICY_ID_COUNTER));
        return uint64(uint256(value));
    }

    /// @notice Returns whether a policy exists on Tempo
    /// @param policyId The ID of the policy to check
    /// @return True if the policy exists, false otherwise
    function policyExists(uint64 policyId) public view returns (bool) {
        // Special policies 0 and 1 always exist
        if (policyId < 2) {
            return true;
        }
        // Check if policy ID is within the range of created policies
        return policyId < policyIdCounter();
    }

    /// @notice Checks if a user is authorized under a specific policy on Tempo
    /// @dev This is the main entry point for zone TIP-20 contracts to enforce policies.
    ///      The calling transaction MUST declare the required Tempo storage slots.
    /// @param policyId The ID of the policy to check against
    /// @param user The address to check authorization for
    /// @return True if the user is authorized, false otherwise
    function isAuthorized(uint64 policyId, address user) public view returns (bool) {
        // Special case for the "always-allow" and "always-reject" policies
        if (policyId < 2) {
            // policyId == 0 is the "always-reject" policy
            // policyId == 1 is the "always-allow" policy
            return policyId == 1;
        }

        // Read policy data from Tempo
        (PolicyType policyType, ) = policyData(policyId);

        // Read whether user is in the policy set
        bool inPolicySet = _isInPolicySet(policyId, user);

        // For whitelist: authorized if in set
        // For blacklist: authorized if NOT in set
        return policyType == PolicyType.WHITELIST ? inPolicySet : !inPolicySet;
    }

    /// @notice Returns the policy data for a given policy ID from Tempo
    /// @param policyId The ID of the policy to query
    /// @return policyType The type of the policy (whitelist or blacklist)
    /// @return admin The admin address of the policy
    function policyData(uint64 policyId)
        public
        view
        returns (PolicyType policyType, address admin)
    {
        if (!policyExists(policyId)) revert PolicyNotFound();

        // Calculate storage slot for _policyData[policyId]
        // mapping slot = keccak256(key, baseSlot)
        bytes32 slot = keccak256(abi.encode(policyId, SLOT_POLICY_DATA_MAPPING));
        bytes32 value = _readTempoSlot(slot);

        // PolicyData is packed: policyType (uint8) in low bits, admin (address) in next 160 bits
        // Solidity packs structs tightly: PolicyType (1 byte) + admin (20 bytes) = 21 bytes in slot
        policyType = PolicyType(uint8(uint256(value)));
        admin = address(uint160(uint256(value) >> 8));
    }

    /*//////////////////////////////////////////////////////////////
                            INTERNAL HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Check if an account is in a policy's set on Tempo
    /// @dev Reads from the policySet mapping: policySet[policyId][account]
    function _isInPolicySet(uint64 policyId, address account) internal view returns (bool) {
        // Calculate storage slot for policySet[policyId][account]
        // Nested mapping: keccak256(innerKey, keccak256(outerKey, baseSlot))
        bytes32 innerSlot = keccak256(abi.encode(policyId, SLOT_POLICY_SET_MAPPING));
        bytes32 slot = keccak256(abi.encode(account, innerSlot));
        bytes32 value = _readTempoSlot(slot);
        return value != bytes32(0);
    }

    /// @notice Read a storage slot from Tempo via the TempoState precompile
    /// @dev This call will revert if the slot was not declared in the transaction's
    ///      TempoStateDeclaration, making the transaction invalid.
    function _readTempoSlot(bytes32 slot) internal view returns (bytes32) {
        // Call TempoState.readTempoStorageSlot(TEMPO_REGISTRY, slot)
        (bool success, bytes memory result) = TEMPO_STATE.staticcall(
            abi.encodeWithSignature(
                "readTempoStorageSlot(address,bytes32)",
                TEMPO_REGISTRY,
                slot
            )
        );
        require(success, "Tempo state read failed");
        return abi.decode(result, (bytes32));
    }

    /*//////////////////////////////////////////////////////////////
                        WRITE FUNCTIONS (DISABLED)
    //////////////////////////////////////////////////////////////*/

    /// @notice Write functions are disabled on zones - policies are managed on Tempo
    function createPolicy(address, PolicyType) external pure returns (uint64) {
        revert("ZoneTIP403Registry: read-only on zones, manage policies on Tempo");
    }

    function createPolicyWithAccounts(address, PolicyType, address[] calldata)
        external
        pure
        returns (uint64)
    {
        revert("ZoneTIP403Registry: read-only on zones, manage policies on Tempo");
    }

    function setPolicyAdmin(uint64, address) external pure {
        revert("ZoneTIP403Registry: read-only on zones, manage policies on Tempo");
    }

    function modifyPolicyWhitelist(uint64, address, bool) external pure {
        revert("ZoneTIP403Registry: read-only on zones, manage policies on Tempo");
    }

    function modifyPolicyBlacklist(uint64, address, bool) external pure {
        revert("ZoneTIP403Registry: read-only on zones, manage policies on Tempo");
    }
}
