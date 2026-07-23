// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC20Like } from "../interfaces/external/IERC20Like.sol";
import { Ownable } from "@openzeppelin/contracts/access/Ownable.sol";
import { Ownable2Step } from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice Minimal EarnVault surface needed by the standalone contribution controller.
interface IContributionTarget {
    function asset() external view returns (address);
    function contribute(uint256 assets) external returns (uint256 venueShares);
    function totalEarnShares() external view returns (uint256);
}

/// @title EarnContributionController
/// @notice Standalone orchestration for one-way Earn backing contributions.
/// @dev The owner chooses a funder and amount for each contribution. The funder keeps custody and
///      approves this contract; funds move atomically from that funder through the immutable EarnVault's
///      permissionless contribution path. No EarnShare is minted. Reward policy and provider identity
///      stay outside the EarnVault, so this controller can be paused or replaced independently.
contract EarnContributionController is Ownable2Step {
    IContributionTarget public immutable earnVault;
    address public immutable asset;

    bool public active = true;

    uint256 private locked = 1;

    event ActiveSet(bool active);
    event Funded(address indexed funder, uint256 requestedAssets, uint256 fundedAssets);

    error Inactive();
    error NotSelf();
    error ReentrantCall();
    error EarnShareSupplyOutOfBounds(uint256 earnShareSupply, uint256 maxEarnShareSupply);
    error TokenCallFailed();
    error TokenCallFalse();
    error ZeroAddress();

    modifier nonReentrant() {
        if (locked != 1) revert ReentrantCall();
        locked = 2;
        _;
        locked = 1;
    }

    constructor(IContributionTarget earnVault_, address owner_) Ownable(owner_) {
        if (address(earnVault_) == address(0)) revert ZeroAddress();

        address asset_ = earnVault_.asset();
        if (asset_ == address(0)) revert ZeroAddress();

        earnVault = earnVault_;
        asset = asset_;
        emit ActiveSet(true);
    }

    /// @notice Pulls one best-effort contribution from `funder` through the EarnVault.
    /// @dev A supply above the owner's authorized maximum, failed provider transfer, or failed engine
    ///      deposit is recorded as zero funding. A lower nonzero supply remains eligible and receives
    ///      a larger contribution per EarnShare.
    /// @param maxEarnShareSupply Maximum live EarnShare supply authorized for this funding attempt. Zero
    ///        guarantees that the attempt records no funding even if the vault is seeded first.
    function fund(address funder, uint256 requestedAssets, uint256 maxEarnShareSupply)
        external
        onlyOwner
        nonReentrant
        returns (uint256 fundedAssets)
    {
        if (!active) revert Inactive();
        if (funder == address(0)) revert ZeroAddress();
        if (requestedAssets == 0) {
            emit Funded(funder, 0, 0);
            return 0;
        }

        // Isolate the provider pull + underlying investment in one atomic subcall. A failed
        // provider or underlying vault records zero without retaining assets in this controller.
        try this.executeFunding(funder, requestedAssets, maxEarnShareSupply) returns (uint256 actualAssets) {
            fundedAssets = actualAssets;
        } catch { }

        emit Funded(funder, requestedAssets, fundedAssets);
    }

    /// @notice Atomic provider pull, live-supply check, and contribution leg used only through `fund`.
    function executeFunding(address funder, uint256 requestedAssets, uint256 maxEarnShareSupply)
        external
        returns (uint256 fundedAssets)
    {
        if (msg.sender != address(this)) revert NotSelf();

        _safeTransferFrom(asset, funder, address(this), requestedAssets);
        uint256 earnShareSupply = earnVault.totalEarnShares();
        if (earnShareSupply == 0 || earnShareSupply > maxEarnShareSupply) {
            revert EarnShareSupplyOutOfBounds(earnShareSupply, maxEarnShareSupply);
        }
        _safeApprove(asset, address(earnVault), requestedAssets);
        earnVault.contribute(requestedAssets);
        _safeApprove(asset, address(earnVault), 0);
        fundedAssets = requestedAssets;
    }

    /// @notice Stops or restarts future funding calls without touching the vault or existing rewards.
    function setActive(bool active_) external onlyOwner {
        if (active == active_) return;
        active = active_;
        emit ActiveSet(active_);
    }

    function _safeTransferFrom(address token, address from, address to, uint256 amount) private {
        (bool ok, bytes memory result) = token.call(abi.encodeCall(IERC20Like.transferFrom, (from, to, amount)));
        if (!ok) revert TokenCallFailed();
        if (result.length != 0 && !abi.decode(result, (bool))) revert TokenCallFalse();
    }

    function _safeApprove(address token, address spender, uint256 amount) private {
        (bool ok, bytes memory result) = token.call(abi.encodeCall(IERC20Like.approve, (spender, amount)));
        if (!ok) revert TokenCallFailed();
        if (result.length != 0 && !abi.decode(result, (bool))) revert TokenCallFalse();
    }
}
