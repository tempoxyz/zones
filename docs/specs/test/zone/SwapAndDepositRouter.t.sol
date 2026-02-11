// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { TIP20 } from "../../src/TIP20.sol";
import { IERC20 } from "../../src/interfaces/IERC20.sol";
import { IStablecoinDEX } from "../../src/interfaces/IStablecoinDEX.sol";
import {
    EncryptedDepositPayload,
    IWithdrawalReceiver,
    IZoneFactory,
    IZoneMessenger,
    IZonePortal
} from "../../src/zone/IZone.sol";
import { SwapAndDepositRouter } from "../../src/zone/SwapAndDepositRouter.sol";
import { BaseTest } from "../BaseTest.t.sol";

contract MockStablecoinDEXForRouter {

    uint128 public nextAmountOut;
    bool public shouldRevert;

    function setNextAmountOut(uint128 _amountOut) external {
        nextAmountOut = _amountOut;
    }

    function setShouldRevert(bool _shouldRevert) external {
        shouldRevert = _shouldRevert;
    }

    function swapExactAmountIn(
        address tokenIn,
        address tokenOut,
        uint128 amountIn,
        uint128 minAmountOut
    )
        external
        returns (uint128 amountOut)
    {
        if (shouldRevert || nextAmountOut < minAmountOut) {
            revert IStablecoinDEX.InsufficientOutput();
        }
        IERC20(tokenIn).transferFrom(msg.sender, address(this), amountIn);
        amountOut = nextAmountOut;
        TIP20(tokenOut).mint(msg.sender, amountOut);
    }

}

contract MockZoneFactoryForRouter {

    mapping(address => bool) public portalMap;
    mapping(address => bool) public messengerMap;

    function setPortal(address portal, bool registered) external {
        portalMap[portal] = registered;
    }

    function setMessenger(address messenger, bool registered) external {
        messengerMap[messenger] = registered;
    }

    function isZonePortal(address portal) external view returns (bool) {
        return portalMap[portal];
    }

    function isZoneMessenger(address messenger) external view returns (bool) {
        return messengerMap[messenger];
    }

}

contract MockZoneMessengerForRouter {

    address public tokenAddr;

    function setToken(address _token) external {
        tokenAddr = _token;
    }

    function token() external view returns (address) {
        return tokenAddr;
    }

}

contract MockZonePortalForRouter {

    address public tokenAddr;

    address public lastDepositRecipient;
    uint128 public lastDepositAmount;
    bytes32 public lastDepositMemo;
    bool public depositCalled;

    uint128 public lastEncryptedAmount;
    uint256 public lastEncryptedKeyIndex;
    bool public encryptedDepositCalled;

    function setToken(address _token) external {
        tokenAddr = _token;
    }

    function token() external view returns (address) {
        return tokenAddr;
    }

    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32) {
        IERC20(tokenAddr).transferFrom(msg.sender, address(this), amount);
        lastDepositRecipient = to;
        lastDepositAmount = amount;
        lastDepositMemo = memo;
        depositCalled = true;
        return bytes32(0);
    }

    function depositEncrypted(
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata
    )
        external
        returns (bytes32)
    {
        IERC20(tokenAddr).transferFrom(msg.sender, address(this), amount);
        lastEncryptedAmount = amount;
        lastEncryptedKeyIndex = keyIndex;
        encryptedDepositCalled = true;
        return bytes32(0);
    }

}

contract SwapAndDepositRouterTest is BaseTest {

    SwapAndDepositRouter public router;
    MockStablecoinDEXForRouter public mockDEX;
    MockZoneFactoryForRouter public mockFactory;
    MockZoneMessengerForRouter public mockMessenger;
    MockZonePortalForRouter public mockPortal;
    MockZonePortalForRouter public mockPortal2;

    address public sender = address(0x500);
    uint128 public constant AMOUNT = 1000e6;

    function setUp() public override {
        super.setUp();

        mockDEX = new MockStablecoinDEXForRouter();
        mockFactory = new MockZoneFactoryForRouter();
        mockMessenger = new MockZoneMessengerForRouter();
        mockPortal = new MockZonePortalForRouter();
        mockPortal2 = new MockZonePortalForRouter();

        router = new SwapAndDepositRouter(address(mockDEX), address(mockFactory));

        mockFactory.setMessenger(address(mockMessenger), true);
        mockFactory.setPortal(address(mockPortal), true);
        mockFactory.setPortal(address(mockPortal2), true);

        mockPortal.setToken(address(pathUSD));
        mockPortal2.setToken(address(token1));

        mockMessenger.setToken(address(pathUSD));

        vm.startPrank(pathUSDAdmin);
        pathUSD.grantRole(_ISSUER_ROLE, pathUSDAdmin);
        pathUSD.mint(address(router), AMOUNT * 10);
        vm.stopPrank();

        vm.prank(admin);
        token1.grantRole(_ISSUER_ROLE, admin);
        vm.prank(admin);
        token1.mint(address(router), AMOUNT * 10);

        vm.prank(admin);
        token1.grantRole(_ISSUER_ROLE, address(mockDEX));
    }

    function _buildPlaintextData(
        address tokenOut,
        address targetPortal,
        address recipient,
        bytes32 memo,
        uint128 minAmountOut
    )
        internal
        pure
        returns (bytes memory)
    {
        return abi.encode(false, tokenOut, targetPortal, recipient, memo, minAmountOut);
    }

    function _buildEncryptedData(
        address tokenOut,
        address targetPortal,
        uint256 keyIndex,
        EncryptedDepositPayload memory encrypted,
        uint128 minAmountOut
    )
        internal
        pure
        returns (bytes memory)
    {
        return abi.encode(true, tokenOut, targetPortal, keyIndex, encrypted, minAmountOut);
    }

    function _defaultEncryptedPayload() internal pure returns (EncryptedDepositPayload memory) {
        return EncryptedDepositPayload({
            ephemeralPubkeyX: bytes32(uint256(0x1234)),
            ephemeralPubkeyYParity: 0x02,
            ciphertext: hex"deadbeef",
            nonce: bytes12(uint96(42)),
            tag: bytes16(uint128(99))
        });
    }

    function test_revertUnauthorizedMessenger() public {
        bytes memory data =
            _buildPlaintextData(address(pathUSD), address(mockPortal), alice, bytes32("memo"), 0);

        vm.prank(alice);
        vm.expectRevert(SwapAndDepositRouter.UnauthorizedMessenger.selector);
        router.onWithdrawalReceived(sender, AMOUNT, data);
    }

    function test_revertInvalidTargetPortal() public {
        address fakePortal = address(0xFAFAFA);
        bytes memory data =
            _buildPlaintextData(address(pathUSD), fakePortal, alice, bytes32("memo"), 0);

        vm.prank(address(mockMessenger));
        vm.expectRevert(SwapAndDepositRouter.InvalidTargetPortal.selector);
        router.onWithdrawalReceived(sender, AMOUNT, data);
    }

    function test_revertInvalidToken() public {
        bytes memory data =
            _buildPlaintextData(address(token1), address(mockPortal), alice, bytes32("memo"), 0);

        vm.prank(address(mockMessenger));
        vm.expectRevert(SwapAndDepositRouter.InvalidToken.selector);
        router.onWithdrawalReceived(sender, AMOUNT, data);
    }

    function test_plaintextDeposit_sameToken() public {
        bytes memory data = _buildPlaintextData(
            address(pathUSD), address(mockPortal), alice, bytes32("hello"), 0
        );

        vm.prank(address(mockMessenger));
        bytes4 ret = router.onWithdrawalReceived(sender, AMOUNT, data);

        assertEq(ret, IWithdrawalReceiver.onWithdrawalReceived.selector);
        assertTrue(mockPortal.depositCalled());
        assertEq(mockPortal.lastDepositRecipient(), alice);
        assertEq(mockPortal.lastDepositAmount(), AMOUNT);
        assertEq(mockPortal.lastDepositMemo(), bytes32("hello"));
    }

    function test_plaintextDeposit_withSwap() public {
        mockMessenger.setToken(address(pathUSD));
        uint128 swapOut = 990e6;
        mockDEX.setNextAmountOut(swapOut);

        bytes memory data = _buildPlaintextData(
            address(token1), address(mockPortal2), alice, bytes32("swap"), 900e6
        );

        vm.prank(address(mockMessenger));
        bytes4 ret = router.onWithdrawalReceived(sender, AMOUNT, data);

        assertEq(ret, IWithdrawalReceiver.onWithdrawalReceived.selector);
        assertTrue(mockPortal2.depositCalled());
        assertEq(mockPortal2.lastDepositRecipient(), alice);
        assertEq(mockPortal2.lastDepositAmount(), swapOut);
        assertEq(mockPortal2.lastDepositMemo(), bytes32("swap"));
    }

    function test_encryptedDeposit_sameToken() public {
        EncryptedDepositPayload memory payload = _defaultEncryptedPayload();
        bytes memory data =
            _buildEncryptedData(address(pathUSD), address(mockPortal), 0, payload, 0);

        vm.prank(address(mockMessenger));
        bytes4 ret = router.onWithdrawalReceived(sender, AMOUNT, data);

        assertEq(ret, IWithdrawalReceiver.onWithdrawalReceived.selector);
        assertTrue(mockPortal.encryptedDepositCalled());
        assertEq(mockPortal.lastEncryptedAmount(), AMOUNT);
        assertEq(mockPortal.lastEncryptedKeyIndex(), 0);
    }

    function test_encryptedDeposit_withSwap() public {
        mockMessenger.setToken(address(pathUSD));
        uint128 swapOut = 950e6;
        mockDEX.setNextAmountOut(swapOut);

        EncryptedDepositPayload memory payload = _defaultEncryptedPayload();
        bytes memory data =
            _buildEncryptedData(address(token1), address(mockPortal2), 1, payload, 900e6);

        vm.prank(address(mockMessenger));
        bytes4 ret = router.onWithdrawalReceived(sender, AMOUNT, data);

        assertEq(ret, IWithdrawalReceiver.onWithdrawalReceived.selector);
        assertTrue(mockPortal2.encryptedDepositCalled());
        assertEq(mockPortal2.lastEncryptedAmount(), swapOut);
        assertEq(mockPortal2.lastEncryptedKeyIndex(), 1);
    }

    function test_swapSlippageReverts() public {
        mockMessenger.setToken(address(pathUSD));
        mockDEX.setNextAmountOut(800e6);

        bytes memory data = _buildPlaintextData(
            address(token1), address(mockPortal2), alice, bytes32("slip"), 900e6
        );

        vm.prank(address(mockMessenger));
        vm.expectRevert(IStablecoinDEX.InsufficientOutput.selector);
        router.onWithdrawalReceived(sender, AMOUNT, data);
    }

}
