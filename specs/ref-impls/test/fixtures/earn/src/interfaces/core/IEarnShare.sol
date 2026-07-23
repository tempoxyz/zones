// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC20Like } from "../external/IERC20Like.sol";

/// @notice Vault-backed TIP-20 EarnShare surface used by `EarnVault`.
/// @dev `burn` burns from the caller's own balance. `EarnVault` must hold the exclusive
///      TIP20 issuer (mint/burn) role so supply can only change through deposit/exit paths.
interface IEarnShare is IERC20Like {
    function burn(uint256 amount) external;
    function decimals() external view returns (uint8);
    function mint(address to, uint256 amount) external;
    function totalSupply() external view returns (uint256);
}
