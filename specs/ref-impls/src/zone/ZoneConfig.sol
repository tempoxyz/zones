// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    ITempoState,
    IZoneConfig,
    PORTAL_ENCRYPTION_KEYS_SLOT,
    PORTAL_PENDING_SEQUENCER_SLOT,
    PORTAL_SEQUENCER_SLOT,
    PORTAL_TOKEN_CONFIGS_SLOT
} from "./IZone.sol";

/// @title ZoneConfig
/// @notice Central zone metadata and L1 state references
/// @dev System contract predeploy at 0x1c00000000000000000000000000000000000003
///      Provides single source of truth for zone configuration.
///      Reads sequencer from L1 ZonePortal, eliminating duplicate sequencer management.
contract ZoneConfig is IZoneConfig {

    /*//////////////////////////////////////////////////////////////
                               IMMUTABLES
    //////////////////////////////////////////////////////////////*/

    /// @notice L1 ZonePortal address
    address public immutable tempoPortal;

    /// @notice TempoState predeploy for L1 reads
    ITempoState public immutable tempoState;

    /*//////////////////////////////////////////////////////////////
                               ERRORS
    //////////////////////////////////////////////////////////////*/

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _tempoPortal, address _tempoState) {
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
        bytes32 value = tempoState.readTempoStorageSlot(tempoPortal, PORTAL_SEQUENCER_SLOT);
        return address(uint160(uint256(value)));
    }

    /// @notice Get pending sequencer by reading from L1 ZonePortal
    /// @dev Reads portal's pendingSequencer slot from finalized Tempo state.
    /// @return Pending sequencer address (0 if none)
    function pendingSequencer() external view returns (address) {
        bytes32 value = tempoState.readTempoStorageSlot(tempoPortal, PORTAL_PENDING_SEQUENCER_SLOT);
        return address(uint160(uint256(value)));
    }

    /// @notice Get sequencer's current encryption public key by reading from L1 ZonePortal
    /// @dev Reads the last entry from the _encryptionKeys dynamic array (slot 6).
    ///      Each EncryptionKeyEntry occupies 2 storage slots:
    ///        slot base + (index * 2):     x (bytes32)
    ///        slot base + (index * 2) + 1: yParity (uint8) + activationBlock (uint64) [packed]
    ///      where base = keccak256(abi.encode(6))
    /// @return x X-coordinate of sequencer's secp256k1 public key
    /// @return yParity Y-coordinate parity (0x02 or 0x03)
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity) {
        // Read the array length from the array's base slot
        uint256 length =
            uint256(tempoState.readTempoStorageSlot(tempoPortal, PORTAL_ENCRYPTION_KEYS_SLOT));

        if (length == 0) revert NoEncryptionKeySet();

        // Compute the storage base for array data: keccak256(abi.encode(slot))
        uint256 base = uint256(keccak256(abi.encode(uint256(PORTAL_ENCRYPTION_KEYS_SLOT))));

        // Read the last entry (2 slots per EncryptionKeyEntry)
        uint256 lastIndex = length - 1;
        uint256 slotX = base + (lastIndex * 2);
        uint256 slotMeta = slotX + 1;

        x = tempoState.readTempoStorageSlot(tempoPortal, bytes32(slotX));
        bytes32 metaSlot = tempoState.readTempoStorageSlot(tempoPortal, bytes32(slotMeta));
        // yParity is packed in the lowest byte of the meta slot (see EncryptionKeyEntry layout)
        yParity = uint8(uint256(metaSlot) & 0xff);
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

    /*//////////////////////////////////////////////////////////////
                         TOKEN REGISTRY ACCESS
    //////////////////////////////////////////////////////////////*/

    /// @notice Check if a token is enabled by reading from L1 ZonePortal
    /// @dev Reads the TokenConfig.enabled field from the portal's _tokenConfigs mapping.
    ///      Mapping storage slot: keccak256(abi.encode(token, PORTAL_TOKEN_CONFIGS_SLOT))
    ///      TokenConfig is packed: enabled (bool, byte 0) and depositsActive (bool, byte 1)
    function isEnabledToken(address token) external view returns (bool) {
        bytes32 configSlot = keccak256(abi.encode(token, PORTAL_TOKEN_CONFIGS_SLOT));
        bytes32 value = tempoState.readTempoStorageSlot(tempoPortal, configSlot);
        // TokenConfig.enabled is the first bool in the struct (lowest byte)
        return uint8(uint256(value) & 0xff) != 0;
    }

}
