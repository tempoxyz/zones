// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity >=0.8.13 <0.9.0;

import { IERC20 } from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import { ITIP20RolesAuth, ITIP20RolesAuthErr } from "./ITIP20RolesAuth.sol";

interface ITIP20 is IERC20, ITIP20RolesAuthErr {
    event Burn(address indexed from, uint256 amount);
    event Mint(address indexed to, uint256 amount);
    event TransferWithMemo(address indexed from, address indexed to, uint256 amount, bytes32 indexed memo);

    function BURN_BLOCKED_ROLE() external view returns (bytes32);
    function ISSUER_ROLE() external view returns (bytes32);
    function PAUSE_ROLE() external view returns (bytes32);
    function UNPAUSE_ROLE() external view returns (bytes32);

    function burn(uint256 amount) external;
    function burnWithMemo(uint256 amount, bytes32 memo) external;
    function currency() external view returns (string memory);
    function decimals() external pure returns (uint8);
    function mint(address to, uint256 amount) external;
    function mintWithMemo(address to, uint256 amount, bytes32 memo) external;
    function name() external view returns (string memory);
    function quoteToken() external view returns (ITIP20);
    function symbol() external view returns (string memory);
    function transferFromWithMemo(address from, address to, uint256 amount, bytes32 memo) external returns (bool);
    function transferPolicyId() external view returns (uint64);
    function transferWithMemo(address to, uint256 amount, bytes32 memo) external;
}

interface ITIP20Token is ITIP20, ITIP20RolesAuth { }
