// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Optional async redemption capability, discoverable through ERC-165.
/// @dev The queue's own ticket always pays the ENGINE (the engine files the request as itself), so
///      the underlying venue never learns end users exist and the one-customer model is preserved.
///      `requestData` is interpreted by the concrete engine; stable adapter accounting does not
///      encode venue-specific queue terms.
interface IVaultEngineAsync {

    function requestRedeem(
        uint256 shares,
        bytes calldata requestData
    )
        external
        returns (bytes32 requestId);

    function cancelRedeem(bytes32 requestId) external returns (uint256 shares);

}
