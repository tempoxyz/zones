// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    IZonePortal,
    ZONE_FACTORY_ADDRESS,
    ZoneAccessMode,
    ZoneGatewayMode,
    ZoneInfo
} from "../../src/interfaces/IZone.sol";

/// @dev Test-only adapter for the pinned Tempo dev L1, whose native TIP-1091 factory predates
/// the additional ZoneInfo fields used by this repository.
address constant TEST_ZONE_FACTORY_ADAPTER_ADDRESS = 0x5af3000000000000000000000000000000000000;

interface ILegacyZoneFactory {

    struct LegacyZoneInfo {
        uint32 zoneId;
        address portal;
        address admin;
        address[] sequencers;
        uint8 threshold;
        address verifier;
        string rpcUrl;
    }

    function zones(uint32 zoneId) external view returns (LegacyZoneInfo memory);

    function isZonePortal(address portal) external view returns (bool);

}

contract TestZoneFactoryAdapter {

    ILegacyZoneFactory internal constant LEGACY_FACTORY = ILegacyZoneFactory(ZONE_FACTORY_ADDRESS);

    function zones(uint32 zoneId) external view returns (ZoneInfo memory info) {
        ILegacyZoneFactory.LegacyZoneInfo memory legacy = LEGACY_FACTORY.zones(zoneId);
        info.zoneId = legacy.zoneId;
        info.portal = legacy.portal;
        info.admin = legacy.admin;
        info.sequencers = legacy.sequencers;
        info.threshold = legacy.threshold;
        info.verifier = legacy.verifier;
        info.rpcUrl = legacy.rpcUrl;

        if (legacy.portal != address(0)) {
            info.accessMode = IZonePortal(legacy.portal).accessMode();
            info.gatewayMode = IZonePortal(legacy.portal).gatewayMode();
        } else {
            info.accessMode = ZoneAccessMode.Open;
            info.gatewayMode = ZoneGatewayMode.Open;
        }
    }

    function isZonePortal(address portal) external view returns (bool) {
        return LEGACY_FACTORY.isZonePortal(portal);
    }

}
