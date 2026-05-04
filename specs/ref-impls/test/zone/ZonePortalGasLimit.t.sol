// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZonePortal, Withdrawal } from "../../src/zone/IZone.sol";
import { EMPTY_SENTINEL } from "../../src/zone/WithdrawalQueueLib.sol";
import { ZonePortal } from "../../src/zone/ZonePortal.sol";
import { Test } from "forge-std/Test.sol";

contract MockPortalToken {

    string public name = "Mock USD";
    string public symbol = "mUSD";
    string public currency = "USD";

    function approve(address, uint256) external pure returns (bool) {
        return true;
    }

}

contract ZonePortalGasLimitTest is Test {

    uint256 internal constant WITHDRAWAL_QUEUE_TAIL_SLOT = 10;
    uint256 internal constant WITHDRAWAL_QUEUE_SLOTS_MAPPING_SLOT = 11;

    ZonePortal public portal;
    MockPortalToken public token;

    address public fallbackRecipient = address(0x200);
    address public recipient = address(0x300);

    function setUp() public {
        token = new MockPortalToken();
        portal = new ZonePortal(
            1,
            address(token),
            address(0x400),
            address(this),
            address(0),
            keccak256("genesis"),
            uint64(block.number)
        );
    }

    function test_processWithdrawal_overMaxGasLimit_bouncesBackAndClearsQueue() public {
        Withdrawal memory w = Withdrawal({
            token: address(token),
            senderTag: keccak256("sender"),
            to: recipient,
            amount: 500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: portal.MAX_WITHDRAWAL_GAS_LIMIT() + 1,
            fallbackRecipient: fallbackRecipient,
            callbackData: "test",
            encryptedSender: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.store(address(portal), bytes32(WITHDRAWAL_QUEUE_TAIL_SLOT), bytes32(uint256(1)));
        vm.store(address(portal), _withdrawalQueueSlot(0), wHash);

        vm.expectEmit(true, false, false, true, address(portal));
        emit IZonePortal.WithdrawalProcessed(recipient, address(token), 500e6, false);
        portal.processWithdrawal(w, bytes32(0));

        assertEq(portal.withdrawalQueueHead(), 1);
        assertEq(portal.withdrawalQueueSlot(0), EMPTY_SENTINEL);
        assertTrue(portal.currentDepositQueueHash() != bytes32(0));
    }

    function _withdrawalQueueSlot(uint256 slot) internal pure returns (bytes32) {
        return keccak256(abi.encode(slot, WITHDRAWAL_QUEUE_SLOTS_MAPPING_SLOT));
    }

}
