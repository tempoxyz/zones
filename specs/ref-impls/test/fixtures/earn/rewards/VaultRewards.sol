// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC20Like } from "../interfaces/IERC20Like.sol";
import { Ownable } from "@openzeppelin/contracts/access/Ownable.sol";
import { Ownable2Step } from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice Minimal adapter surface needed by the standalone reward controller.
interface IRewardAdapter {

    function asset() external view returns (address);
    function contribute(uint256 assets) external returns (uint256 venueShares);
    function shareSupply() external view returns (uint256);

}

/// @title VaultRewards
/// @notice Standalone orchestration for one-way Earn backing contributions.
/// @dev The owner chooses a funder and amount for each contribution. The funder keeps custody and
///      approves this contract; funds move atomically from that funder through the immutable adapter's
///      permissionless contribution path. No EarnToken is minted. Reward policy and provider identity
///      stay outside the adapter, so this controller can be paused or replaced independently.
contract VaultRewards is Ownable2Step {

    IRewardAdapter public immutable adapter;
    address public immutable asset;

    bool public active = true;

    uint256 private locked = 1;

    event ActiveSet(bool active);
    event Funded(address indexed funder, uint256 requested, uint256 funded);

    error Inactive();
    error NotSelf();
    error ReentrantCall();
    error TokenCallFailed();
    error TokenCallFalse();
    error ZeroAddress();

    modifier nonReentrant() {
        if (locked != 1) revert ReentrantCall();
        locked = 2;
        _;
        locked = 1;
    }

    constructor(IRewardAdapter adapter_, address owner_) Ownable(owner_) {
        if (address(adapter_) == address(0)) revert ZeroAddress();

        address asset_ = adapter_.asset();
        if (asset_ == address(0)) revert ZeroAddress();

        adapter = adapter_;
        asset = asset_;
        emit ActiveSet(true);
    }

    /// @notice Pulls one best-effort contribution from `funder` through the Earn adapter.
    /// @dev A failed provider transfer or engine deposit is recorded as zero funding.
    function fund(
        address funder,
        uint256 requested
    )
        external
        onlyOwner
        nonReentrant
        returns (uint256 funded)
    {
        if (!active) revert Inactive();
        if (funder == address(0)) revert ZeroAddress();
        if (requested == 0) {
            emit Funded(funder, 0, 0);
            return 0;
        }
        if (adapter.shareSupply() == 0) {
            emit Funded(funder, requested, 0);
            return 0;
        }

        // Isolate the provider pull + underlying investment in one atomic subcall. A failed
        // provider or underlying vault records zero without retaining assets in this controller.
        try this.executeFunding(funder, requested) returns (uint256 actual) {
            funded = actual;
        } catch { }

        emit Funded(funder, requested, funded);
    }

    /// @notice Atomic funding leg used only through `fund` so failures can be recorded as zero.
    function executeFunding(address funder, uint256 requested) external returns (uint256 funded) {
        if (msg.sender != address(this)) revert NotSelf();

        _safeTransferFrom(asset, funder, address(this), requested);
        _safeApprove(asset, address(adapter), requested);
        // forge-lint: disable-next-line(unused-return)
        adapter.contribute(requested);
        _safeApprove(asset, address(adapter), 0);
        funded = requested;
    }

    /// @notice Stops or restarts future funding calls without touching the vault or existing rewards.
    function setActive(bool active_) external onlyOwner {
        if (active == active_) return;
        active = active_;
        emit ActiveSet(active_);
    }

    function _safeTransferFrom(address token, address from, address to, uint256 amount) private {
        (bool ok, bytes memory result) =
            token.call(abi.encodeCall(IERC20Like.transferFrom, (from, to, amount)));
        if (!ok) revert TokenCallFailed();
        if (result.length != 0 && !abi.decode(result, (bool))) revert TokenCallFalse();
    }

    function _safeApprove(address token, address spender, uint256 amount) private {
        (bool ok, bytes memory result) =
            token.call(abi.encodeCall(IERC20Like.approve, (spender, amount)));
        if (!ok) revert TokenCallFailed();
        if (result.length != 0 && !abi.decode(result, (bool))) revert TokenCallFalse();
    }

}
