// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Deployment-fixed policy for the EarnVault's whole-pool engine migration seam.
enum EngineMigrationMode {
    UserOnly,
    OperatorEnabled
}

/// @notice Initialization-time emergency and liveness roles for one Earn deployment.
struct EarnVaultControls {
    /// @dev Optional fast risk-reducing seat. May pause new exposure, but never unpause or move backing.
    ///      Set to zero for operator-only pause authority.
    address emergencyGuardian;
    /// @dev Optional narrow liveness seat. May cancel open async requests only to their stored receivers.
    ///      Set to zero for requester-only cancellation.
    address asyncJanitor;
    /// @dev Fixed forever at EarnVault initialization; there is intentionally no setter.
    EngineMigrationMode migrationMode;
}
