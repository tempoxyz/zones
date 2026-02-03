// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZoneToken, ITempoState, IL1StateSubscriptionManager } from "./IZone.sol";

/// @title ZoneConfig
/// @notice Central zone metadata and L1 state references
/// @dev Predeploy at 0x1c00000000000000000000000000000000000002
///      Provides single source of truth for zone configuration and L1 reads.
///      Eliminates duplication of sequencer transfer logic and zone metadata across contracts.
contract ZoneConfig {
    /*//////////////////////////////////////////////////////////////
                               IMMUTABLES
    //////////////////////////////////////////////////////////////*/

    /// @notice Zone token address (same on L1 and L2)
    address public immutable zoneToken;

    /// @notice L1 ZonePortal address
    address public immutable l1Portal;

    /// @notice L1 TIP-403 registry address
    address public immutable l1TIP403Registry;

    /// @notice TempoState predeploy for L1 reads
    ITempoState public immutable tempoState;

    /// @notice L1StateSubscriptionManager for subscription checks
    IL1StateSubscriptionManager public immutable subscriptionManager;

    /*//////////////////////////////////////////////////////////////
                           STORAGE SLOT CONSTANTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Storage slot for sequencer in ZonePortal
    /// @dev ZonePortal storage layout (non-immutable variables):
    ///      slot 0: sequencer (address)
    ///      slot 1: pendingSequencer (address)
    ///      ...
    bytes32 internal constant SEQUENCER_SLOT = bytes32(uint256(0));

    /// @notice Storage slot for pendingSequencer in ZonePortal
    bytes32 internal constant PENDING_SEQUENCER_SLOT = bytes32(uint256(1));

    /*//////////////////////////////////////////////////////////////
                               ERRORS
    //////////////////////////////////////////////////////////////*/

    error NotSequencer();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(
        address _zoneToken,
        address _l1Portal,
        address _l1TIP403Registry,
        address _tempoState,
        address _subscriptionManager
    ) {
        zoneToken = _zoneToken;
        l1Portal = _l1Portal;
        l1TIP403Registry = _l1TIP403Registry;
        tempoState = ITempoState(_tempoState);
        subscriptionManager = IL1StateSubscriptionManager(_subscriptionManager);
    }

    /*//////////////////////////////////////////////////////////////
                          L1 STATE ACCESSORS
    //////////////////////////////////////////////////////////////*/

    /// @notice Get current sequencer by reading from L1 ZonePortal
    /// @dev Reads portal's sequencer slot via L1 state subscription.
    ///      This is automatically subscribed at genesis (permanent).
    ///      Returns the authoritative sequencer address from L1.
    /// @return Current sequencer address
    function sequencer() external view returns (address) {
        bytes32 value = tempoState.readTempoStorageSlot(l1Portal, SEQUENCER_SLOT);
        return address(uint160(uint256(value)));
    }

    /// @notice Get pending sequencer by reading from L1 ZonePortal
    /// @dev Reads portal's pendingSequencer slot via L1 state subscription.
    ///      This is automatically subscribed at genesis (permanent).
    /// @return Pending sequencer address (0 if none)
    function pendingSequencer() external view returns (address) {
        bytes32 value = tempoState.readTempoStorageSlot(l1Portal, PENDING_SEQUENCER_SLOT);
        return address(uint160(uint256(value)));
    }

    /*//////////////////////////////////////////////////////////////
                              MODIFIERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Modifier to restrict access to current sequencer
    /// @dev Reads sequencer from L1 via ZonePortal for each check.
    ///      L1 is the single source of truth.
    modifier onlySequencer() {
        if (msg.sender != this.sequencer()) revert NotSequencer();
        _;
    }

    /*//////////////////////////////////////////////////////////////
                          CONVENIENCE GETTERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Check if an address is the current sequencer
    /// @param account Address to check
    /// @return True if account is the current sequencer
    function isSequencer(address account) external view returns (bool) {
        return account == this.sequencer();
    }

    /// @notice Get zone token as IZoneToken interface
    /// @return Zone token interface
    function getZoneToken() external view returns (IZoneToken) {
        return IZoneToken(zoneToken);
    }
}
