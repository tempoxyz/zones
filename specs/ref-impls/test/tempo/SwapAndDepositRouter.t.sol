// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    EncryptedDepositPayload,
    IWithdrawalReceiver,
    IZoneFactory,
    IZonePortal,
    ZONE_MESSENGER_ADDRESS,
    ZoneInfo
} from "../../src/interfaces/IZone.sol";
import { SwapAndDepositRouter } from "../../src/tempo/SwapAndDepositRouter.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { IStablecoinDEX } from "tempo-std/interfaces/IStablecoinDEX.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";

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
        ITIP20(tokenIn).transferFrom(msg.sender, address(this), amountIn);
        amountOut = nextAmountOut;
        ITIP20(tokenOut).mint(msg.sender, amountOut);
    }

}

contract MockZoneFactoryForRouter {

    mapping(address => bool) public portalMap;
    mapping(uint32 => ZoneInfo) internal _zones;

    function setPortal(address portal, bool registered) external {
        portalMap[portal] = registered;
    }

    function setSourcePortal(uint32 zoneId, address portal) external {
        _zones[zoneId].zoneId = zoneId;
        _zones[zoneId].portal = portal;
    }

    function isZonePortal(address portal) external view returns (bool) {
        return portalMap[portal];
    }

    function zones(uint32 id) external view returns (ZoneInfo memory) {
        return _zones[id];
    }

}

contract MockZonePortalForRouter {

    mapping(address => bool) public enabledTokens;

    address public lastDepositRecipient;
    address public lastDepositBouncebackRecipient;
    uint128 public lastDepositAmount;
    bytes32 public lastDepositMemo;
    bool public depositCalled;

    uint128 public lastEncryptedAmount;
    uint256 public lastEncryptedKeyIndex;
    address public lastEncryptedBouncebackRecipient;
    bool public encryptedDepositCalled;

    function enableToken(address _token) external {
        enabledTokens[_token] = true;
    }

    function isTokenEnabled(address _token) external view returns (bool) {
        return enabledTokens[_token];
    }

    function deposit(
        address _token,
        address to,
        uint128 amount,
        bytes32 memo,
        address tempoRefundRecipient
    )
        external
        returns (bytes32)
    {
        ITIP20(_token).transferFrom(msg.sender, address(this), amount);
        lastDepositRecipient = to;
        lastDepositBouncebackRecipient = tempoRefundRecipient;
        lastDepositAmount = amount;
        lastDepositMemo = memo;
        depositCalled = true;
        return bytes32(0);
    }

    function depositEncrypted(
        address _token,
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata,
        address tempoRefundRecipient
    )
        external
        returns (bytes32)
    {
        ITIP20(_token).transferFrom(msg.sender, address(this), amount);
        lastEncryptedAmount = amount;
        lastEncryptedKeyIndex = keyIndex;
        lastEncryptedBouncebackRecipient = tempoRefundRecipient;
        encryptedDepositCalled = true;
        return bytes32(0);
    }

}

contract SwapAndDepositRouterTest is BaseTest {

    SwapAndDepositRouter public router;
    MockStablecoinDEXForRouter public mockDEX;
    MockZoneFactoryForRouter public mockFactory;
    MockZonePortalForRouter public mockPortal;
    MockZonePortalForRouter public mockPortal2;

    uint32 public constant SOURCE_ZONE_ID = 7;
    bytes32 public senderTag = keccak256(abi.encodePacked(address(0x500)));
    address public sourcePortal = address(0x501);
    address public refundBurner = address(0xb000000000000000000000000000000000000123);
    uint128 public constant AMOUNT = 1000e6;

    function setUp() public override {
        super.setUp();

        mockDEX = new MockStablecoinDEXForRouter();
        mockFactory = new MockZoneFactoryForRouter();
        mockPortal = new MockZonePortalForRouter();
        mockPortal2 = new MockZonePortalForRouter();

        router = new SwapAndDepositRouter(address(mockDEX), address(mockFactory));

        mockFactory.setSourcePortal(SOURCE_ZONE_ID, sourcePortal);
        mockFactory.setPortal(address(mockPortal), true);
        mockFactory.setPortal(address(mockPortal2), true);

        mockPortal.enableToken(address(pathUSD));
        mockPortal2.enableToken(address(token1));

        vm.startPrank(pathUSDAdmin);
        pathUSD.grantRole(_ISSUER_ROLE, pathUSDAdmin);
        pathUSD.mint(address(router), AMOUNT * 10);
        vm.stopPrank();

        vm.prank(sequencer);
        token1.grantRole(_ISSUER_ROLE, sequencer);
        vm.prank(sequencer);
        token1.mint(address(router), AMOUNT * 10);

        vm.prank(sequencer);
        token1.grantRole(_ISSUER_ROLE, address(mockDEX));
    }

    function _buildPlaintextData(
        address tokenOut,
        address targetPortal,
        address recipient,
        address tempoRefundRecipient,
        bytes32 memo,
        uint128 minAmountOut
    )
        internal
        pure
        returns (bytes memory)
    {
        return abi.encode(
            false, tokenOut, targetPortal, recipient, tempoRefundRecipient, memo, minAmountOut
        );
    }

    function _buildEncryptedData(
        address tokenOut,
        address targetPortal,
        uint256 keyIndex,
        EncryptedDepositPayload memory encrypted,
        address tempoRefundRecipient,
        uint128 minAmountOut
    )
        internal
        pure
        returns (bytes memory)
    {
        return abi.encode(
            true, tokenOut, targetPortal, keyIndex, encrypted, tempoRefundRecipient, minAmountOut
        );
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
        bytes memory data = _buildPlaintextData(
            address(pathUSD), address(mockPortal), alice, refundBurner, bytes32("memo"), 0
        );

        vm.prank(alice);
        vm.expectRevert(SwapAndDepositRouter.UnauthorizedMessenger.selector);
        router.onWithdrawalReceived(
            SOURCE_ZONE_ID, sourcePortal, senderTag, address(pathUSD), AMOUNT, data
        );
    }

    function test_revertInvalidSourcePortal() public {
        bytes memory data = _buildPlaintextData(
            address(pathUSD), address(mockPortal), alice, refundBurner, bytes32("memo"), 0
        );

        vm.prank(ZONE_MESSENGER_ADDRESS);
        vm.expectRevert(SwapAndDepositRouter.InvalidSourcePortal.selector);
        router.onWithdrawalReceived(
            SOURCE_ZONE_ID, address(0xBAD), senderTag, address(pathUSD), AMOUNT, data
        );
    }

    function test_revertInvalidTargetPortal() public {
        address fakePortal = address(0xFAFAFA);
        bytes memory data = _buildPlaintextData(
            address(pathUSD), fakePortal, alice, refundBurner, bytes32("memo"), 0
        );

        vm.prank(ZONE_MESSENGER_ADDRESS);
        vm.expectRevert(SwapAndDepositRouter.InvalidTargetPortal.selector);
        router.onWithdrawalReceived(
            SOURCE_ZONE_ID, sourcePortal, senderTag, address(pathUSD), AMOUNT, data
        );
    }

    function test_revertInvalidToken() public {
        bytes memory data = _buildPlaintextData(
            address(token1), address(mockPortal), alice, refundBurner, bytes32("memo"), 0
        );

        vm.prank(ZONE_MESSENGER_ADDRESS);
        vm.expectRevert(SwapAndDepositRouter.InvalidToken.selector);
        router.onWithdrawalReceived(
            SOURCE_ZONE_ID, sourcePortal, senderTag, address(pathUSD), AMOUNT, data
        );
    }

    function test_plaintextDeposit_sameToken() public {
        bytes memory data = _buildPlaintextData(
            address(pathUSD), address(mockPortal), alice, refundBurner, bytes32("hello"), 0
        );

        vm.prank(ZONE_MESSENGER_ADDRESS);
        bytes4 ret = router.onWithdrawalReceived(
            SOURCE_ZONE_ID, sourcePortal, senderTag, address(pathUSD), AMOUNT, data
        );

        assertEq(ret, IWithdrawalReceiver.onWithdrawalReceived.selector);
        assertTrue(mockPortal.depositCalled());
        assertEq(mockPortal.lastDepositRecipient(), alice);
        assertEq(mockPortal.lastDepositBouncebackRecipient(), refundBurner);
        assertEq(mockPortal.lastDepositAmount(), AMOUNT);
        assertEq(mockPortal.lastDepositMemo(), bytes32("hello"));
    }

    function test_plaintextDeposit_withSwap() public {
        uint128 swapOut = 990e6;
        mockDEX.setNextAmountOut(swapOut);

        bytes memory data = _buildPlaintextData(
            address(token1), address(mockPortal2), alice, refundBurner, bytes32("swap"), 900e6
        );

        vm.prank(ZONE_MESSENGER_ADDRESS);
        bytes4 ret = router.onWithdrawalReceived(
            SOURCE_ZONE_ID, sourcePortal, senderTag, address(pathUSD), AMOUNT, data
        );

        assertEq(ret, IWithdrawalReceiver.onWithdrawalReceived.selector);
        assertTrue(mockPortal2.depositCalled());
        assertEq(mockPortal2.lastDepositRecipient(), alice);
        assertEq(mockPortal2.lastDepositBouncebackRecipient(), refundBurner);
        assertEq(mockPortal2.lastDepositAmount(), swapOut);
        assertEq(mockPortal2.lastDepositMemo(), bytes32("swap"));
    }

    function test_encryptedDeposit_sameToken() public {
        EncryptedDepositPayload memory payload = _defaultEncryptedPayload();
        bytes memory data =
            _buildEncryptedData(address(pathUSD), address(mockPortal), 0, payload, refundBurner, 0);

        vm.prank(ZONE_MESSENGER_ADDRESS);
        bytes4 ret = router.onWithdrawalReceived(
            SOURCE_ZONE_ID, sourcePortal, senderTag, address(pathUSD), AMOUNT, data
        );

        assertEq(ret, IWithdrawalReceiver.onWithdrawalReceived.selector);
        assertTrue(mockPortal.encryptedDepositCalled());
        assertEq(mockPortal.lastEncryptedAmount(), AMOUNT);
        assertEq(mockPortal.lastEncryptedKeyIndex(), 0);
        assertEq(mockPortal.lastEncryptedBouncebackRecipient(), refundBurner);
    }

    function test_encryptedDeposit_withSwap() public {
        uint128 swapOut = 950e6;
        mockDEX.setNextAmountOut(swapOut);

        EncryptedDepositPayload memory payload = _defaultEncryptedPayload();
        bytes memory data = _buildEncryptedData(
            address(token1), address(mockPortal2), 1, payload, refundBurner, 900e6
        );

        vm.prank(ZONE_MESSENGER_ADDRESS);
        bytes4 ret = router.onWithdrawalReceived(
            SOURCE_ZONE_ID, sourcePortal, senderTag, address(pathUSD), AMOUNT, data
        );

        assertEq(ret, IWithdrawalReceiver.onWithdrawalReceived.selector);
        assertTrue(mockPortal2.encryptedDepositCalled());
        assertEq(mockPortal2.lastEncryptedAmount(), swapOut);
        assertEq(mockPortal2.lastEncryptedKeyIndex(), 1);
        assertEq(mockPortal2.lastEncryptedBouncebackRecipient(), refundBurner);
    }

    function test_swapSlippageReverts() public {
        mockDEX.setNextAmountOut(800e6);

        bytes memory data = _buildPlaintextData(
            address(token1), address(mockPortal2), alice, refundBurner, bytes32("slip"), 900e6
        );

        vm.prank(ZONE_MESSENGER_ADDRESS);
        vm.expectRevert(IStablecoinDEX.InsufficientOutput.selector);
        router.onWithdrawalReceived(
            SOURCE_ZONE_ID, sourcePortal, senderTag, address(pathUSD), AMOUNT, data
        );
    }

}
