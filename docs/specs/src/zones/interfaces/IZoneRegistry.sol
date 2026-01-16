// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZoneTypes} from "./IZoneTypes.sol";

/// @title IZoneRegistry
/// @notice Optional registry for zone metadata and batch heads
interface IZoneRegistry is IZoneTypes {
    /// @notice Emitted when a zone is registered
    event ZoneRegistered(uint64 indexed zoneId, address indexed portal);

    /// @notice Emitted when a batch head is updated
    event BatchHeadUpdated(uint64 indexed zoneId, uint64 indexed batchIndex, bytes32 stateRoot);

    error ZoneAlreadyRegistered();
    error ZoneNotFound();
    error OnlyPortal();

    /// @notice Register a zone
    function registerZone(ZoneInfo calldata info) external;

    /// @notice Get zone info by ID
    function getZone(uint64 zoneId) external view returns (ZoneInfo memory);

    /// @notice Get the batch head for a zone
    /// @return batchIndex The current batch index
    /// @return stateRoot The current state root
    function batchHead(uint64 zoneId) external view returns (uint64 batchIndex, bytes32 stateRoot);

    /// @notice Update the batch head (called by portal)
    function updateBatchHead(uint64 zoneId, uint64 batchIndex, bytes32 stateRoot) external;
}
