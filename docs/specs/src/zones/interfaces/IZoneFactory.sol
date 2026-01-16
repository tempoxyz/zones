// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZoneTypes} from "./IZoneTypes.sol";

/// @title IZoneFactory
/// @notice Factory for creating new zones
interface IZoneFactory is IZoneTypes {
    /// @notice Parameters for creating a new zone
    struct CreateZoneParams {
        address gasToken;
        address sequencer;
        address verifier;
        bytes32 genesisStateRoot;
    }

    /// @notice Emitted when a new zone is created
    event ZoneCreated(
        uint64 indexed zoneId,
        address indexed portal,
        address indexed gasToken,
        address sequencer,
        address verifier,
        bytes32 genesisStateRoot
    );

    /// @notice Create a new zone
    /// @param params The zone creation parameters
    /// @return zoneId The ID of the newly created zone
    /// @return portal The address of the zone's portal contract
    function createZone(CreateZoneParams calldata params) external returns (uint64 zoneId, address portal);

    /// @notice Get the total number of zones created
    /// @return The zone count
    function zoneCount() external view returns (uint64);

    /// @notice Get zone info by ID
    /// @param zoneId The zone ID
    /// @return The zone info
    function zones(uint64 zoneId) external view returns (ZoneInfo memory);

    /// @notice Check if an address is a zone portal
    /// @param portal The address to check
    /// @return True if the address is a zone portal
    function isZonePortal(address portal) external view returns (bool);
}
