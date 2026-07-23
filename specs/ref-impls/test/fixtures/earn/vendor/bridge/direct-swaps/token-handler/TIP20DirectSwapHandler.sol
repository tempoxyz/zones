// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import { ITokenHandler } from "./ITokenHandler.sol";
import { ITIP20Controller } from "../../tip20-controller/interfaces/ITIP20Controller.sol";
import { AccessControl } from "@openzeppelin/contracts/access/AccessControl.sol";
import { IERC20 } from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import { SafeERC20 } from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import { IERC165 } from "@openzeppelin/contracts/utils/introspection/IERC165.sol";
import { Pausable } from "@openzeppelin/contracts/utils/Pausable.sol";
import { ReentrancyGuardTransient } from "@openzeppelin/contracts/utils/ReentrancyGuardTransient.sol";

contract TIP20DirectSwapHandler is ITokenHandler, AccessControl, Pausable, ReentrancyGuardTransient {
    using SafeERC20 for IERC20;

    bytes32 public constant DIRECT_SWAP_SETTER_ROLE = keccak256("DIRECT_SWAP_SETTER_ROLE");
    bytes32 public constant PAUSER_ROLE = keccak256("PAUSER_ROLE");

    ITIP20Controller public immutable CONTROLLER;
    address public immutable RESERVE_LEDGER_TOKEN;

    address public directSwapContract;

    error InvalidStablecoinToken();
    error OnlyDirectSwap();
    error ZeroAddress();
    error ZeroAmount();

    event DirectSwapContractUpdated(address indexed oldDirectSwapContract, address indexed newDirectSwapContract);

    constructor(address admin, address controller, address reserveLedgerToken) {
        if (admin == address(0) || controller == address(0) || reserveLedgerToken == address(0)) {
            revert ZeroAddress();
        }

        CONTROLLER = ITIP20Controller(controller);
        RESERVE_LEDGER_TOKEN = reserveLedgerToken;

        _grantRole(DEFAULT_ADMIN_ROLE, admin);
        _grantRole(DIRECT_SWAP_SETTER_ROLE, admin);
        _grantRole(PAUSER_ROLE, admin);
    }

    modifier onlyDirectSwap() {
        if (msg.sender != directSwapContract) {
            revert OnlyDirectSwap();
        }
        _;
    }

    function setDirectSwapContract(address newDirectSwapContract) external onlyRole(DIRECT_SWAP_SETTER_ROLE) {
        if (newDirectSwapContract == address(0)) {
            revert ZeroAddress();
        }

        address oldDirectSwapContract = directSwapContract;
        directSwapContract = newDirectSwapContract;

        emit DirectSwapContractUpdated(oldDirectSwapContract, newDirectSwapContract);
    }

    function pause() external onlyRole(PAUSER_ROLE) {
        _pause();
    }

    function unpause() external onlyRole(PAUSER_ROLE) {
        _unpause();
    }

    function deposit(address token, uint256 amount) external onlyDirectSwap nonReentrant whenNotPaused {
        _validateStablecoin(token, amount);

        IERC20(token).safeTransferFrom(msg.sender, address(this), amount);
        IERC20(token).forceApprove(address(CONTROLLER), amount);

        CONTROLLER.unwrap(token, amount);

        IERC20(RESERVE_LEDGER_TOKEN).safeTransfer(msg.sender, amount);

        emit Deposited(msg.sender, token, msg.sender, amount);
    }

    function withdraw(address token, uint256 amount) external onlyDirectSwap nonReentrant whenNotPaused {
        _validateStablecoin(token, amount);

        IERC20(RESERVE_LEDGER_TOKEN).safeTransferFrom(msg.sender, address(this), amount);
        IERC20(RESERVE_LEDGER_TOKEN).forceApprove(address(CONTROLLER), amount);

        CONTROLLER.wrap(token, msg.sender, amount);

        emit Withdrawn(msg.sender, token, RESERVE_LEDGER_TOKEN, amount);
    }

    function supportsInterface(bytes4 interfaceId) public view override(AccessControl, IERC165) returns (bool) {
        return interfaceId == type(ITokenHandler).interfaceId || super.supportsInterface(interfaceId);
    }

    function _validateStablecoin(address token, uint256 amount) internal view {
        if (token == address(0)) {
            revert ZeroAddress();
        }
        if (token == RESERVE_LEDGER_TOKEN) {
            revert InvalidStablecoinToken();
        }
        if (amount == 0) {
            revert ZeroAmount();
        }
    }
}
