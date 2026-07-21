// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

// ZoneFeeManager replaces Tempo's fee manager at the canonical precompile address.
address constant ZONE_FEE_MANAGER = 0xfeEC000000000000000000000000000000000000;

/// @title IZoneFeeManager
/// @notice Zone-native fee manager with no AMM or fee-token preferences.
interface IZoneFeeManager {

    event FeesDistributed(address indexed sequencer, address indexed token, uint256 amount);

    function collectedFees(address sequencer, address token) external view returns (uint256);
    function distributeFees(address sequencer, address token) external;
    function isEnabledToken(address token) external view returns (bool);

}
