// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import {Test} from "forge-std/Test.sol";

import {SimpleDirectSwapFixture} from "./SimpleDirectSwapFixture.sol";

contract SimpleDirectSwapTestToken {
    mapping(address account => uint256 balance) public balanceOf;
    mapping(address owner => mapping(address spender => uint256 amount)) public allowance;

    error InsufficientAllowance();
    error InsufficientBalance();

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 approved = allowance[from][msg.sender];
        if (approved < amount) revert InsufficientAllowance();
        if (approved != type(uint256).max) {
            allowance[from][msg.sender] = approved - amount;
        }
        _transfer(from, to, amount);
        return true;
    }

    function _transfer(address from, address to, uint256 amount) internal {
        uint256 balance = balanceOf[from];
        if (balance < amount) revert InsufficientBalance();
        balanceOf[from] = balance - amount;
        balanceOf[to] += amount;
    }
}

contract SimpleDirectSwapFixtureTest is Test {
    uint256 internal constant LIQUIDITY = 1_000_000;
    uint256 internal constant SWAP_AMOUNT = 100_000;

    SimpleDirectSwapTestToken internal tokenA;
    SimpleDirectSwapTestToken internal tokenB;
    SimpleDirectSwapTestToken internal tokenC;
    SimpleDirectSwapFixture internal swap;
    address internal user;

    function setUp() public {
        tokenA = new SimpleDirectSwapTestToken();
        tokenB = new SimpleDirectSwapTestToken();
        tokenC = new SimpleDirectSwapTestToken();
        swap = new SimpleDirectSwapFixture(address(tokenA), address(tokenB));
        user = makeAddr("user");

        tokenA.mint(address(swap), LIQUIDITY);
        tokenB.mint(address(swap), LIQUIDITY);
        tokenA.mint(user, LIQUIDITY);
        tokenB.mint(user, LIQUIDITY);
        vm.startPrank(user);
        tokenA.approve(address(swap), type(uint256).max);
        tokenB.approve(address(swap), type(uint256).max);
        tokenC.approve(address(swap), type(uint256).max);
        vm.stopPrank();
    }

    function testSwapsOneForOneInBothDirections() public {
        vm.startPrank(user);
        swap.swapExactIn(address(tokenA), address(tokenB), SWAP_AMOUNT);
        swap.swapExactIn(address(tokenB), address(tokenA), SWAP_AMOUNT);
        vm.stopPrank();

        assertEq(tokenA.balanceOf(user), LIQUIDITY);
        assertEq(tokenB.balanceOf(user), LIQUIDITY);
        assertEq(tokenA.balanceOf(address(swap)), LIQUIDITY);
        assertEq(tokenB.balanceOf(address(swap)), LIQUIDITY);
    }

    function testRejectsInvalidPair() public {
        vm.prank(user);
        vm.expectRevert(SimpleDirectSwapFixture.InvalidToken.selector);
        swap.swapExactIn(address(tokenA), address(tokenC), SWAP_AMOUNT);
    }

    function testRejectsZeroAmount() public {
        vm.prank(user);
        vm.expectRevert(SimpleDirectSwapFixture.ZeroAmount.selector);
        swap.swapExactIn(address(tokenA), address(tokenB), 0);
    }

    function testWrapsInsufficientOutputLiquidityAsTokenCallFailure() public {
        tokenA.mint(user, LIQUIDITY);

        vm.prank(user);
        vm.expectRevert(SimpleDirectSwapFixture.TokenCallFailed.selector);
        swap.swapExactIn(address(tokenA), address(tokenB), LIQUIDITY + 1);
    }
}
