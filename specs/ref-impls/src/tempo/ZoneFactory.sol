// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    IZoneFactory,
    ZONE_MESSENGER_ADDRESS,
    ZONE_VERIFIER_ADDRESS,
    ZoneInfo
} from "../interfaces/IZone.sol";
import { ZonePortal } from "./ZonePortal.sol";
import { StdPrecompiles } from "tempo-std/StdPrecompiles.sol";
import { ITIP20Factory } from "tempo-std/interfaces/ITIP20Factory.sol";

/// @title ZoneFactory
/// @notice Creates zones and registers parameters
contract ZoneFactory is IZoneFactory {

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice Minimum gas required for zone creation.
    /// @dev Prevents low-cost zone spam. The caller must supply at least this much gas.
    uint256 public constant ZONE_CREATION_GAS = 15_000_000;

    /// @notice Next zone ID to be assigned
    /// @dev Starts at 1, reserving zone ID 0 for potential future use (e.g., mainnet as zone 0)
    uint32 public nextZoneId = 1;

    mapping(uint32 => ZoneInfo) internal _zones;
    mapping(address => bool) internal _isZonePortal;
    address public owner;

    /*//////////////////////////////////////////////////////////////
                            ZONE CREATION
    //////////////////////////////////////////////////////////////*/

    function createZone(CreateZoneParams calldata params)
        external
        returns (uint32 zoneId, address portal)
    {
        if (msg.sender != owner) revert NotOwner();

        // Validate initial token is a TIP-20
        if (!ITIP20Factory(StdPrecompiles.TIP20_FACTORY_ADDRESS).isTIP20(params.initialToken)) {
            revert InvalidToken();
        }
        if (params.admin == address(0)) revert InvalidAdmin();
        if (params.sequencer == address(0)) revert InvalidSequencer();
        if (gasleft() < ZONE_CREATION_GAS) revert InsufficientGas();

        zoneId = nextZoneId;
        if (zoneId == type(uint32).max) revert ZoneIdOverflow();
        nextZoneId = zoneId + 1;

        // Deploy and atomically initialize the portal. TIP-1091 fixes this factory's address as
        // the portal's only initializer authority.
        ZonePortal portalContract = new ZonePortal();
        portalContract.initialize(
            zoneId,
            params.initialToken,
            ZONE_MESSENGER_ADDRESS,
            params.admin,
            params.sequencer,
            ZONE_VERIFIER_ADDRESS,
            params.zoneParams.genesisBlockHash,
            params.zoneParams.genesisTempoBlockNumber,
            params.rpcUrl
        );
        portal = address(portalContract);

        // Store zone info
        _zones[zoneId] = ZoneInfo({
            zoneId: zoneId,
            portal: portal,
            initialToken: params.initialToken,
            admin: params.admin,
            sequencer: params.sequencer,
            genesisBlockHash: params.zoneParams.genesisBlockHash,
            genesisTempoBlockHash: params.zoneParams.genesisTempoBlockHash,
            genesisTempoBlockNumber: params.zoneParams.genesisTempoBlockNumber,
            rpcUrl: params.rpcUrl
        });

        _isZonePortal[portal] = true;

        emit ZoneCreated(
            zoneId,
            portal,
            params.initialToken,
            params.admin,
            params.sequencer,
            ZONE_VERIFIER_ADDRESS,
            params.zoneParams.genesisBlockHash,
            params.zoneParams.genesisTempoBlockHash,
            params.zoneParams.genesisTempoBlockNumber
        );
    }

    /// @inheritdoc IZoneFactory
    function transferOwnership(address newOwner) external {
        if (msg.sender != owner) revert NotOwner();
        if (newOwner == address(0)) revert InvalidOwner();

        address previousOwner = owner;
        owner = newOwner;
        emit OwnershipTransferred(previousOwner, newOwner);
    }

    /*//////////////////////////////////////////////////////////////
                                 VIEWS
    //////////////////////////////////////////////////////////////*/

    function zones(uint32 zoneId) external view returns (ZoneInfo memory) {
        return _zones[zoneId];
    }

    function isZonePortal(address portal) external view returns (bool) {
        return _isZonePortal[portal];
    }

}
