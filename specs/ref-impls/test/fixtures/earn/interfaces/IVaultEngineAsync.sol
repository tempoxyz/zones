// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Optional async request/claim extension of the engine seam, discoverable via
///         `supportsAsync()`. Sync/liquid engines (for example a Morpho 4626 passthrough) return
///         `false` and never implement the request/cancel/forward methods; queue-backed engines
///         (Veda) return `true`.
/// @dev The queue's own ticket always pays the ENGINE (the engine files the request as itself), so
///      the underlying venue never learns end users exist and the one-customer model is preserved.
///      `forwardSolved` is callable only by the authorized forwarding solver; it credits a measured,
///      balance-bounded per-asset settled ledger and finalizes each funded request through
///      `VaultAdapter.finalizeRedeem`.
interface IVaultEngineAsync {

    function supportsAsync() external view returns (bool);

    function requestRedeem(
        uint128 shares,
        address assetOut,
        uint16 discountBps,
        uint24 secondsToDeadline
    )
        external
        returns (bytes32 requestId);

    function cancelRedeem(bytes32 requestId) external returns (uint256 shares);

    function forwardSolved(
        bytes32[] calldata requestIds,
        address asset,
        uint256 delta
    )
        external
        returns (uint256 claimed);

}
