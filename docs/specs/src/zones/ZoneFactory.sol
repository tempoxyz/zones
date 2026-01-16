// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZoneFactory} from "./interfaces/IZoneFactory.sol";
import {IZoneRegistry} from "./interfaces/IZoneRegistry.sol";
import {ZonePortal} from "./ZonePortal.sol";

/// @title ZoneFactory
/// @notice Factory for creating new zones
contract ZoneFactory is IZoneFactory {
    /// @notice The zone registry
    IZoneRegistry public immutable registry;

    /// @notice The next zone ID
    uint64 private _nextZoneId;

    /// @notice Zone info by zone ID
    mapping(uint64 => ZoneInfo) private _zones;

    /// @notice Portal address to zone ID
    mapping(address => uint64) private _portalToZone;

    constructor(address registry_) {
        registry = IZoneRegistry(registry_);
        _nextZoneId = 1;
    }

    /// @inheritdoc IZoneFactory
    function createZone(CreateZoneParams calldata params) external returns (uint64 zoneId, address portal) {
        zoneId = _nextZoneId++;

        portal = address(new ZonePortal(
            zoneId,
            params.gasToken,
            params.sequencer,
            params.verifier,
            params.genesisStateRoot,
            address(registry),
            address(this)
        ));

        ZoneInfo memory info = ZoneInfo({
            zoneId: zoneId,
            portal: portal,
            gasToken: params.gasToken,
            sequencer: params.sequencer,
            verifier: params.verifier,
            genesisStateRoot: params.genesisStateRoot
        });

        _zones[zoneId] = info;
        _portalToZone[portal] = zoneId;

        registry.registerZone(info);

        emit ZoneCreated(
            zoneId,
            portal,
            params.gasToken,
            params.sequencer,
            params.verifier,
            params.genesisStateRoot
        );
    }

    /// @inheritdoc IZoneFactory
    function zoneCount() external view returns (uint64) {
        return _nextZoneId - 1;
    }

    /// @inheritdoc IZoneFactory
    function zones(uint64 zoneId) external view returns (ZoneInfo memory) {
        return _zones[zoneId];
    }

    /// @inheritdoc IZoneFactory
    function isZonePortal(address portal) external view returns (bool) {
        return _portalToZone[portal] != 0;
    }
}
