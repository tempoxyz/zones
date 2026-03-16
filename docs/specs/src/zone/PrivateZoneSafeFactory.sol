// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { PrivateZoneSafe } from "./PrivateZoneSafe.sol";

/**
 * @title PrivateZoneSafeProxy
 * @notice Minimal proxy (EIP-1167) that delegates all calls to the PrivateZoneSafe singleton.
 * @dev Created by PrivateZoneSafeFactory via CREATE2. The singleton address is immutable
 *      and encoded into the proxy bytecode at deployment time.
 */
contract PrivateZoneSafeProxy {
    address internal immutable singleton;

    constructor(address _singleton) {
        singleton = _singleton;
    }

    fallback() external payable {
        address impl = singleton;
        assembly {
            calldatacopy(0, 0, calldatasize())
            let result := delegatecall(gas(), impl, 0, calldatasize(), 0, 0)
            returndatacopy(0, 0, returndatasize())
            switch result
            case 0 { revert(0, returndatasize()) }
            default { return(0, returndatasize()) }
        }
    }

    receive() external payable {}
}

/**
 * @title PrivateZoneSafeFactory
 * @notice Factory for deploying PrivateZoneSafe proxies via CREATE2.
 *
 * @dev This factory is deployed by the sequencer and whitelisted in ZoneConfig as an
 * authorized deployer. It is the only non-system contract allowed to execute CREATE2 on
 * the zone.
 *
 * The factory is intentionally minimal:
 *   - One function: createProxy (deploys a proxy and calls setup).
 *   - Deterministic addresses via CREATE2 with a user-chosen salt.
 *   - No ownership, no upgrades, no admin functions.
 *
 * The singleton address is set at construction and cannot be changed. To deploy Safes
 * against a new singleton (e.g., after a protocol upgrade), the sequencer deploys a new
 * factory instance and whitelists it.
 */
contract PrivateZoneSafeFactory {

    /*//////////////////////////////////////////////////////////////
                                EVENTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Emitted when a new Safe proxy is created.
    /// @param proxy Address of the newly deployed proxy.
    /// @param singleton Address of the singleton the proxy delegates to.
    event ProxyCreation(address indexed proxy, address indexed singleton);

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error ProxyCreationFailed();
    error SetupFailed();

    /*//////////////////////////////////////////////////////////////
                               IMMUTABLES
    //////////////////////////////////////////////////////////////*/

    /// @notice The PrivateZoneSafe singleton all proxies delegate to.
    address public immutable singleton;

    /// @notice The fallback handler set during Safe setup.
    address public immutable fallbackHandler;

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    /// @param _singleton Address of the deployed PrivateZoneSafe master copy.
    /// @param _fallbackHandler Address of the compatibility fallback handler.
    constructor(address _singleton, address _fallbackHandler) {
        singleton = _singleton;
        fallbackHandler = _fallbackHandler;
    }

    /*//////////////////////////////////////////////////////////////
                           PROXY DEPLOYMENT
    //////////////////////////////////////////////////////////////*/

    /// @notice Deploy a new Safe proxy and initialize it.
    /// @dev The proxy address is deterministic: it depends on the singleton address,
    ///      the initializer (derived from owners + threshold), and the salt.
    ///
    ///      Users can compute the address before deployment:
    ///        salt = keccak256(abi.encode(keccak256(initializer), userSalt))
    ///        address = CREATE2(factory, salt, keccak256(creationCode))
    ///
    /// @param owners Initial owner addresses for the Safe.
    /// @param _threshold Number of required signatures.
    /// @param userSalt User-chosen salt for address derivation.
    /// @return proxy Address of the deployed Safe proxy.
    function createProxy(address[] calldata owners, uint256 _threshold, bytes32 userSalt)
        external
        returns (address proxy)
    {
        bytes memory initializer =
            abi.encodeCall(PrivateZoneSafe.setup, (owners, _threshold, fallbackHandler));

        bytes32 salt = keccak256(abi.encode(keccak256(initializer), userSalt));
        bytes memory creationCode = abi.encodePacked(type(PrivateZoneSafeProxy).creationCode, abi.encode(singleton));

        assembly {
            proxy := create2(0, add(creationCode, 0x20), mload(creationCode), salt)
        }
        if (proxy == address(0)) revert ProxyCreationFailed();

        emit ProxyCreation(proxy, singleton);

        (bool success,) = proxy.call(initializer);
        if (!success) revert SetupFailed();
    }

    /// @notice Compute the deterministic address of a proxy before deployment.
    /// @param owners Initial owner addresses.
    /// @param _threshold Number of required signatures.
    /// @param userSalt User-chosen salt.
    /// @return predicted The address the proxy would be deployed to.
    function computeAddress(address[] calldata owners, uint256 _threshold, bytes32 userSalt)
        external
        view
        returns (address predicted)
    {
        bytes memory initializer =
            abi.encodeCall(PrivateZoneSafe.setup, (owners, _threshold, fallbackHandler));

        bytes32 salt = keccak256(abi.encode(keccak256(initializer), userSalt));
        bytes32 creationCodeHash =
            keccak256(abi.encodePacked(type(PrivateZoneSafeProxy).creationCode, abi.encode(singleton)));

        predicted = address(
            uint160(uint256(keccak256(abi.encodePacked(bytes1(0xff), address(this), salt, creationCodeHash))))
        );
    }
}
