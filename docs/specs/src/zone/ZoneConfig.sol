// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITempoState, IZoneToken } from "./IZone.sol";

/// @title ZoneConfig
/// @notice Central zone metadata and L1 state references
/// @dev System contract predeploy at 0x1c00000000000000000000000000000000000003
///      Provides single source of truth for zone configuration.
///      Reads sequencer from L1 ZonePortal, eliminating duplicate sequencer management.
contract ZoneConfig {

    /*//////////////////////////////////////////////////////////////
                               IMMUTABLES
    //////////////////////////////////////////////////////////////*/

    /// @notice Zone token address (TIP-20 at same address as Tempo)
    address public immutable zoneToken;

    /// @notice L1 ZonePortal address
    address public immutable tempoPortal;

    /// @notice TempoState predeploy for L1 reads
    ITempoState public immutable tempoState;

    /*//////////////////////////////////////////////////////////////
                           STORAGE SLOT CONSTANTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Storage slot for sequencer in ZonePortal
    /// @dev ZonePortal storage layout (non-immutable variables):
    ///      slot 0: sequencer (address)
    ///      slot 1: pendingSequencer (address)
    ///      slot 2: sequencerPubkey (bytes32)
    ///      ...
    bytes32 internal constant SEQUENCER_SLOT = bytes32(uint256(0));

    /// @notice Storage slot for pendingSequencer in ZonePortal
    bytes32 internal constant PENDING_SEQUENCER_SLOT = bytes32(uint256(1));

    /// @notice Storage slot for sequencerPubkey in ZonePortal (X coordinate)
    bytes32 internal constant SEQUENCER_PUBKEY_SLOT = bytes32(uint256(2));

    /*//////////////////////////////////////////////////////////////
                               ERRORS
    //////////////////////////////////////////////////////////////*/

    error NotSequencer();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _zoneToken, address _tempoPortal, address _tempoState) {
        zoneToken = _zoneToken;
        tempoPortal = _tempoPortal;
        tempoState = ITempoState(_tempoState);
    }

    /*//////////////////////////////////////////////////////////////
                          L1 STATE ACCESSORS
    //////////////////////////////////////////////////////////////*/

    /// @notice Get current sequencer by reading from L1 ZonePortal
    /// @dev Reads portal's sequencer slot from finalized Tempo state.
    ///      L1 ZonePortal is the single source of truth for sequencer.
    ///      Sequencer changes on L1 become visible after Tempo block finalization.
    /// @return Current sequencer address
    function sequencer() external view returns (address) {
        bytes32 value = tempoState.readTempoStorageSlot(tempoPortal, SEQUENCER_SLOT);
        return address(uint160(uint256(value)));
    }

    /// @notice Get pending sequencer by reading from L1 ZonePortal
    /// @dev Reads portal's pendingSequencer slot from finalized Tempo state.
    /// @return Pending sequencer address (0 if none)
    function pendingSequencer() external view returns (address) {
        bytes32 value = tempoState.readTempoStorageSlot(tempoPortal, PENDING_SEQUENCER_SLOT);
        return address(uint160(uint256(value)));
    }

    /// @notice Get sequencer's encryption public key by reading from L1 ZonePortal
    /// @dev Reads portal's sequencerPubkey slot from finalized Tempo state.
    ///      Used for encrypted deposits (ECIES).
    /// @return x X-coordinate of sequencer's secp256k1 public key
    /// @return yParity Y-coordinate parity (0 or 1)
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity) {
        bytes32 value = tempoState.readTempoStorageSlot(tempoPortal, SEQUENCER_PUBKEY_SLOT);
        // Public key is stored as: x (bytes32) with yParity in the first byte
        yParity = uint8(uint256(value) >> 248);
        x = bytes32(uint256(value) & ((1 << 248) - 1));
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
                          CONVENIENCE FUNCTIONS
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
