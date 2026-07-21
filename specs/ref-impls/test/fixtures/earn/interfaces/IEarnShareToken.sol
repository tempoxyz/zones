// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC20Like } from "./IERC20Like.sol";

/// @notice Vault-backed TIP20 share token surface used by `VaultAdapter`.
/// @dev `burn` burns from the caller's own balance. `VaultAdapter` must hold the exclusive
///      TIP20 issuer (mint/burn) role so supply can only change through deposit/exit paths.
interface IEarnShareToken is IERC20Like {

    function burn(uint256 amount) external;
    function decimals() external view returns (uint8);
    function mint(address to, uint256 amount) external;
    function totalSupply() external view returns (uint256);

}
