// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

// ZoneFeeManager precompile address (0x1c00...0006).
address constant ZONE_FEE_MANAGER = 0x1c00000000000000000000000000000000000006;

/// @title IZoneFeeManager
/// @notice Zone-native fee manager with no AMM or validator-token preference.
interface IZoneFeeManager {
    event UserTokenSet(address indexed user, address indexed token);
    event FeesDistributed(address indexed sequencer, address indexed token, uint256 amount);

    function userTokens(address user) external view returns (address);
    function collectedFees(address sequencer, address token) external view returns (uint256);
    function setUserToken(address token) external;
    function distributeFees(address sequencer, address token) external;
    function isEnabledToken(address token) external view returns (bool);
}
