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

    /// @notice Next zone ID to be assigned
    /// @dev Starts at 1, reserving zone ID 0 for potential future use (e.g., mainnet as zone 0)
    uint32 public nextZoneId = 1;
    address public owner;
    bool public implementationUpdatesLocked;

    mapping(uint32 => ZoneInfo) internal _zones;
    mapping(address => bool) internal _isZonePortal;

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor() {
        owner = msg.sender;
        emit OwnershipTransferred(address(0), msg.sender);
    }

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
        _validateSequencerSet(params.sequencers, params.threshold);

        zoneId = nextZoneId;
        nextZoneId = zoneId + 1;

        // Deploy and atomically initialize the portal. TIP-1091 fixes this factory's address as
        // the portal's only initializer authority.
        ZonePortal portalContract = new ZonePortal();
        portalContract.initialize(
            zoneId,
            params.initialToken,
            ZONE_MESSENGER_ADDRESS,
            params.admin,
            params.sequencers,
            params.threshold,
            ZONE_VERIFIER_ADDRESS,
            params.rpcUrl
        );
        portal = address(portalContract);

        // Store zone info
        _zones[zoneId] = ZoneInfo({
            zoneId: zoneId,
            portal: portal,
            admin: params.admin,
            sequencers: params.sequencers,
            threshold: params.threshold,
            verifier: ZONE_VERIFIER_ADDRESS,
            rpcUrl: params.rpcUrl
        });

        _isZonePortal[portal] = true;

        emit ZoneCreated(
            zoneId,
            portal,
            params.initialToken,
            params.admin,
            params.sequencers,
            params.threshold,
            ZONE_VERIFIER_ADDRESS
        );
    }

    /// @inheritdoc IZoneFactory
    function transferOwnership(address newOwner) external {
        if (msg.sender != owner) revert NotOwner();

        address previousOwner = owner;
        owner = newOwner;
        emit OwnershipTransferred(previousOwner, newOwner);
    }

    function lockImplementationUpdates() external {
        if (msg.sender != owner) revert NotOwner();
        implementationUpdatesLocked = true;
    }

    /// @dev Runtime copying is a native TIP-1091 operation and cannot be represented in EVM
    ///      Solidity. These methods exist only to keep this deployable test reference ABI-aligned.
    function setPortalImplementation(address) external {
        if (msg.sender != owner) revert NotOwner();
        if (implementationUpdatesLocked) revert ImplementationUpdatesLocked();
        revert InvalidPortalImplementation();
    }

    function setZoneMessengerImplementation(address) external {
        if (msg.sender != owner) revert NotOwner();
        if (implementationUpdatesLocked) revert ImplementationUpdatesLocked();
        revert InvalidZoneMessengerImplementation();
    }

    function setVerifierImplementation(address) external {
        if (msg.sender != owner) revert NotOwner();
        if (implementationUpdatesLocked) revert ImplementationUpdatesLocked();
        revert InvalidVerifierImplementation();
    }

    function _validateSequencerSet(address[] calldata sequencers, uint8 threshold) internal pure {
        uint256 length = sequencers.length;
        if (length == 0 || length > 8 || threshold == 0 || threshold > length) {
            revert InvalidSequencerSet();
        }

        for (uint256 i = 0; i < length; ++i) {
            address current = sequencers[i];
            if (current == address(0)) revert InvalidSequencerSet();
            for (uint256 j = 0; j < i; ++j) {
                if (sequencers[j] == current) revert InvalidSequencerSet();
            }
        }
    }

    /*//////////////////////////////////////////////////////////////
                                 VIEWS
    //////////////////////////////////////////////////////////////*/

    function zones(uint32 id) external view returns (ZoneInfo memory info) {
        return _zones[id];
    }

    function isZonePortal(address portal) external view returns (bool) {
        return _isZonePortal[portal];
    }

}
