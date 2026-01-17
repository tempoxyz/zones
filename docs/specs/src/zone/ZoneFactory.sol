// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZoneFactory, ZoneInfo, IVerifier } from "./IZone.sol";
import { ZonePortal } from "./ZonePortal.sol";
import { TempoUtilities } from "../TempoUtilities.sol";

/// @title ZoneFactory
/// @notice Creates zones and registers parameters
contract ZoneFactory is IZoneFactory {
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    uint64 internal _zoneCount;
    mapping(uint64 => ZoneInfo) internal _zones;
    mapping(address => bool) internal _isZonePortal;

    /*//////////////////////////////////////////////////////////////
                            ZONE CREATION
    //////////////////////////////////////////////////////////////*/

    function createZone(CreateZoneParams calldata params) external returns (uint64 zoneId, address portal) {
        // Validate token is a TIP-20
        if (!TempoUtilities.isTIP20(params.token)) revert InvalidToken();
        if (params.sequencer == address(0)) revert InvalidSequencer();
        if (params.verifier == address(0)) revert InvalidVerifier();

        zoneId = _zoneCount++;

        // Deploy portal
        ZonePortal portalContract = new ZonePortal(
            zoneId,
            params.token,
            params.sequencer,
            params.verifier,
            params.genesisStateRoot
        );
        portal = address(portalContract);

        // Store zone info
        _zones[zoneId] = ZoneInfo({
            zoneId: zoneId,
            portal: portal,
            token: params.token,
            sequencer: params.sequencer,
            verifier: params.verifier,
            genesisStateRoot: params.genesisStateRoot
        });

        _isZonePortal[portal] = true;

        emit ZoneCreated(
            zoneId,
            portal,
            params.token,
            params.sequencer,
            params.verifier,
            params.genesisStateRoot
        );
    }

    /*//////////////////////////////////////////////////////////////
                                 VIEWS
    //////////////////////////////////////////////////////////////*/

    function zoneCount() external view returns (uint64) {
        return _zoneCount;
    }

    function zones(uint64 zoneId) external view returns (ZoneInfo memory) {
        return _zones[zoneId];
    }

    function isZonePortal(address portal) external view returns (bool) {
        return _isZonePortal[portal];
    }
}
