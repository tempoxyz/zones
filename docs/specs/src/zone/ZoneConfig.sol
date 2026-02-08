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
    ///      slot 2: sequencerPubkey (bytes32, legacy)
    ///      slot 3: zoneGasRate (uint128) + withdrawalBatchIndex (uint64) [packed]
    ///      slot 4: blockHash (bytes32)
    ///      slot 5: currentDepositQueueHash (bytes32)
    ///      slot 6: lastSyncedTempoBlockNumber (uint64)
    ///      slot 7: _encryptionKeys (EncryptionKeyEntry[])
    bytes32 internal constant SEQUENCER_SLOT = bytes32(uint256(0));

    /// @notice Storage slot for pendingSequencer in ZonePortal
    bytes32 internal constant PENDING_SEQUENCER_SLOT = bytes32(uint256(1));

    /// @notice Storage slot for _encryptionKeys dynamic array in ZonePortal
    /// @dev Each EncryptionKeyEntry occupies 2 storage slots:
    ///      slot base + (index * 2):     x (bytes32)
    ///      slot base + (index * 2) + 1: yParity (uint8) + activationBlock (uint64) [packed]
    ///      where base = keccak256(abi.encode(7))
    bytes32 internal constant ENCRYPTION_KEYS_SLOT = bytes32(uint256(7));

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

    /// @notice Get sequencer's current encryption public key by reading from L1 ZonePortal
    /// @dev Reads the last entry from the _encryptionKeys dynamic array (slot 7).
    ///      Each EncryptionKeyEntry occupies 2 storage slots:
    ///        slot base + (index * 2):     x (bytes32)
    ///        slot base + (index * 2) + 1: yParity (uint8) + activationBlock (uint64) [packed]
    ///      where base = keccak256(abi.encode(7))
    /// @return x X-coordinate of sequencer's secp256k1 public key
    /// @return yParity Y-coordinate parity (0 or 1)
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity) {
        // Read the array length from the array's base slot
        uint256 length = uint256(tempoState.readTempoStorageSlot(tempoPortal, ENCRYPTION_KEYS_SLOT));

        if (length == 0) return (bytes32(0), 0);

        // Compute the storage base for array data: keccak256(abi.encode(slot))
        uint256 base = uint256(keccak256(abi.encode(uint256(ENCRYPTION_KEYS_SLOT))));

        // Read the last entry (2 slots per EncryptionKeyEntry)
        uint256 lastIndex = length - 1;
        uint256 slotX = base + (lastIndex * 2);
        uint256 slotMeta = slotX + 1;

        x = tempoState.readTempoStorageSlot(tempoPortal, bytes32(slotX));
        bytes32 metaSlot = tempoState.readTempoStorageSlot(tempoPortal, bytes32(slotMeta));
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

    /// @notice Get zone token as IZoneToken interface
    /// @return Zone token interface
    function getZoneToken() external view returns (IZoneToken) {
        return IZoneToken(zoneToken);
    }

}
