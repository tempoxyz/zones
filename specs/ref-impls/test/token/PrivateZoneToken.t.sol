// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITempoState, IZoneConfig, ZONE_INBOX, ZONE_OUTBOX } from "../../src/interfaces/IZone.sol";
import { PrivateZoneToken } from "../../src/token/PrivateZoneToken.sol";
import { Test } from "forge-std/Test.sol";

contract MockPrivateZoneTokenConfig is IZoneConfig {

    address public currentSequencer;

    constructor(address sequencer_) {
        currentSequencer = sequencer_;
    }

    function setSequencer(address sequencer_) external {
        currentSequencer = sequencer_;
    }

    function tempoPortal() external pure returns (address) {
        return address(0x400);
    }

    function tempoState() external pure returns (ITempoState) {
        return ITempoState(address(0x401));
    }

    function sequencer() external view returns (address) {
        return currentSequencer;
    }

    function pendingSequencer() external pure returns (address) {
        return address(0);
    }

    function sequencerEncryptionKey() external pure returns (bytes32, uint8) {
        return (bytes32(0), 0);
    }

    function isSequencer(address account) external view returns (bool) {
        return account == currentSequencer;
    }

    function isEnabledToken(address) external pure returns (bool) {
        return false;
    }

}

contract PrivateZoneTokenHarness is PrivateZoneToken {

    constructor(IZoneConfig config_) {
        config = config_;
    }

}

contract PrivateZoneTokenTest is Test {

    PrivateZoneTokenHarness public token;
    MockPrivateZoneTokenConfig public config;
    address public sequencer = address(0x100);
    address public alice = address(0x200);
    address public bob = address(0x300);
    address public charlie = address(0x400);

    function setUp() public {
        config = new MockPrivateZoneTokenConfig(sequencer);
        token = new PrivateZoneTokenHarness(config);
    }

    /// @notice Verifies the fixed transfer gas constant is 100,000.
    function test_fixedTransferGasConstant() public view {
        assertEq(token.FIXED_TRANSFER_GAS(), 100_000);
    }

    /// @notice Verifies non-owner non-sequencer balance reads are unauthorized.
    function test_balanceOf_revertsUnauthorizedForNonOwnerNonSequencer() public {
        vm.prank(charlie);
        vm.expectRevert(PrivateZoneToken.Unauthorized.selector);
        token.balanceOf(alice);
    }

    /// @notice Verifies unrelated callers cannot read allowances.
    function test_allowance_revertsUnauthorizedForUnrelatedCaller() public {
        vm.prank(charlie);
        vm.expectRevert(PrivateZoneToken.Unauthorized.selector);
        token.allowance(alice, bob);
    }

    /// @notice Verifies minting is restricted to the zone inbox caller.
    function test_mint_revertsUnauthorizedForNonInbox() public {
        vm.prank(alice);
        vm.expectRevert(PrivateZoneToken.Unauthorized.selector);
        token.mint(alice, 1);
    }

    /// @notice Verifies burning is restricted to the zone outbox caller.
    function test_burn_revertsUnauthorizedForNonOutbox() public {
        vm.prank(alice);
        vm.expectRevert(PrivateZoneToken.Unauthorized.selector);
        token.burn(alice, 1);
    }

    /// @notice Verifies authorized privacy reads reach the precompile stub.
    function test_authorizedPrivacyReadsReachPrecompileStub() public {
        vm.prank(alice);
        vm.expectRevert();
        token.balanceOf(alice);

        vm.prank(sequencer);
        vm.expectRevert();
        token.allowance(alice, bob);
    }

    /// @notice Verifies inbox mint and outbox burn calls reach the precompile stub.
    function test_systemMintBurnCallersReachPrecompileStub() public {
        vm.prank(ZONE_INBOX);
        vm.expectRevert();
        token.mint(alice, 1);

        vm.prank(ZONE_OUTBOX);
        vm.expectRevert();
        token.burn(alice, 1);
    }

}
