// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZoneRegistry} from "./interfaces/IZoneRegistry.sol";
import {IZoneTypes} from "./interfaces/IZoneTypes.sol";

/// @title ZoneRegistry
/// @notice Registry for zone metadata and batch heads
contract ZoneRegistry is IZoneRegistry {
    /// @notice Zone info by zone ID
    mapping(uint64 => ZoneInfo) private _zones;

    /// @notice Batch head by zone ID
    mapping(uint64 => BatchHead) private _batchHeads;

    /// @notice Portal address to zone ID mapping
    mapping(address => uint64) private _portalToZone;

    struct BatchHead {
        uint64 batchIndex;
        bytes32 stateRoot;
        bytes32 exitRoot;
    }

    /// @inheritdoc IZoneRegistry
    function registerZone(ZoneInfo calldata info) external {
        if (_zones[info.zoneId].portal != address(0)) {
            revert ZoneAlreadyRegistered();
        }

        _zones[info.zoneId] = info;
        _portalToZone[info.portal] = info.zoneId;
        _batchHeads[info.zoneId] = BatchHead({
            batchIndex: 0,
            stateRoot: info.genesisStateRoot,
            exitRoot: bytes32(0)
        });

        emit ZoneRegistered(info.zoneId, info.portal);
    }

    /// @inheritdoc IZoneRegistry
    function getZone(uint64 zoneId) external view returns (ZoneInfo memory) {
        if (_zones[zoneId].portal == address(0)) {
            revert ZoneNotFound();
        }
        return _zones[zoneId];
    }

    /// @inheritdoc IZoneRegistry
    function batchHead(uint64 zoneId)
        external
        view
        returns (uint64 batchIndex, bytes32 stateRoot, bytes32 exitRoot)
    {
        BatchHead storage head = _batchHeads[zoneId];
        return (head.batchIndex, head.stateRoot, head.exitRoot);
    }

    /// @inheritdoc IZoneRegistry
    function updateBatchHead(
        uint64 zoneId,
        uint64 batchIndex_,
        bytes32 stateRoot,
        bytes32 exitRoot
    ) external {
        ZoneInfo storage info = _zones[zoneId];
        if (info.portal == address(0)) {
            revert ZoneNotFound();
        }
        if (msg.sender != info.portal) {
            revert OnlyPortal();
        }

        _batchHeads[zoneId] = BatchHead({
            batchIndex: batchIndex_,
            stateRoot: stateRoot,
            exitRoot: exitRoot
        });

        emit BatchHeadUpdated(zoneId, batchIndex_, stateRoot, exitRoot);
    }
}
