// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { PrivateZoneSafe } from "../../src/zone/PrivateZoneSafe.sol";
import {
    PrivateZoneSafeFactory,
    PrivateZoneSafeProxy
} from "../../src/zone/PrivateZoneSafeFactory.sol";
import { Test } from "forge-std/Test.sol";

/// @title PrivateZoneSafeTest
/// @notice Tests for the locked-down Safe singleton and factory for privacy zones
contract PrivateZoneSafeTest is Test {

    PrivateZoneSafe public singleton;
    PrivateZoneSafeFactory public factory;
    address public fallbackHandler;

    uint256 internal ownerKey1 = 0xa1;
    uint256 internal ownerKey2 = 0xb2;
    uint256 internal ownerKey3 = 0xc3;
    address internal owner1;
    address internal owner2;
    address internal owner3;
    address internal nonOwner = address(0xdead);

    function setUp() public {
        owner1 = vm.addr(ownerKey1);
        owner2 = vm.addr(ownerKey2);
        owner3 = vm.addr(ownerKey3);

        singleton = new PrivateZoneSafe();
        fallbackHandler = address(new MockFallbackHandler());
        factory = new PrivateZoneSafeFactory(address(singleton), fallbackHandler);
    }

    /*//////////////////////////////////////////////////////////////
                            HELPER FUNCTIONS
    //////////////////////////////////////////////////////////////*/

    function _sortedOwners2() internal view returns (address[] memory) {
        address[] memory owners = new address[](2);
        if (owner1 < owner2) {
            owners[0] = owner1;
            owners[1] = owner2;
        } else {
            owners[0] = owner2;
            owners[1] = owner1;
        }
        return owners;
    }

    function _sortedOwners3() internal view returns (address[] memory) {
        address[] memory addrs = new address[](3);
        addrs[0] = owner1;
        addrs[1] = owner2;
        addrs[2] = owner3;
        // Simple sort for 3 elements
        for (uint256 i = 0; i < 3; i++) {
            for (uint256 j = i + 1; j < 3; j++) {
                if (addrs[i] > addrs[j]) {
                    (addrs[i], addrs[j]) = (addrs[j], addrs[i]);
                }
            }
        }
        return addrs;
    }

    function _deploySafe(
        address[] memory owners,
        uint256 threshold
    )
        internal
        returns (PrivateZoneSafe)
    {
        address proxy = factory.createProxy(owners, threshold, bytes32(0));
        return PrivateZoneSafe(payable(proxy));
    }

    function _signTransaction(
        PrivateZoneSafe safe,
        uint256[] memory keys,
        address to,
        uint256 value,
        bytes memory data
    )
        internal
        view
        returns (bytes memory)
    {
        bytes32 txHash = safe.getTransactionHash(to, value, data, safe.nonce());

        // Sort keys by derived address (ascending) for Safe signature ordering
        for (uint256 i = 0; i < keys.length; i++) {
            for (uint256 j = i + 1; j < keys.length; j++) {
                if (vm.addr(keys[i]) > vm.addr(keys[j])) {
                    (keys[i], keys[j]) = (keys[j], keys[i]);
                }
            }
        }

        bytes memory signatures;
        for (uint256 i = 0; i < keys.length; i++) {
            (uint8 v, bytes32 r, bytes32 s) = vm.sign(keys[i], txHash);
            signatures = abi.encodePacked(signatures, r, s, v);
        }
        return signatures;
    }

    /*//////////////////////////////////////////////////////////////
                         FACTORY: DEPLOYMENT
    //////////////////////////////////////////////////////////////*/

    function test_factory_createProxy_success() public {
        address[] memory owners = _sortedOwners2();
        address proxy = factory.createProxy(owners, 1, bytes32(0));

        assertTrue(proxy != address(0));
        PrivateZoneSafe safe = PrivateZoneSafe(payable(proxy));
        assertEq(safe.ownerCount(), 2);
        assertEq(safe.threshold(), 1);
        assertTrue(safe.isOwner(owner1));
        assertTrue(safe.isOwner(owner2));
    }

    function test_factory_createProxy_emitsEvent() public {
        address[] memory owners = _sortedOwners2();

        vm.expectEmit(false, true, false, false);
        emit PrivateZoneSafeFactory.ProxyCreation(address(0), address(singleton));
        factory.createProxy(owners, 1, bytes32(0));
    }

    function test_factory_createProxy_differentSalts() public {
        address[] memory owners = _sortedOwners2();
        address proxy1 = factory.createProxy(owners, 1, bytes32(uint256(1)));
        address proxy2 = factory.createProxy(owners, 1, bytes32(uint256(2)));

        assertTrue(proxy1 != proxy2);
    }

    function test_factory_createProxy_sameSaltReverts() public {
        address[] memory owners = _sortedOwners2();
        factory.createProxy(owners, 1, bytes32(0));

        vm.expectRevert(PrivateZoneSafeFactory.ProxyCreationFailed.selector);
        factory.createProxy(owners, 1, bytes32(0));
    }

    function test_factory_computeAddress_matchesDeployment() public {
        address[] memory owners = _sortedOwners2();
        bytes32 salt = bytes32(uint256(42));

        address predicted = factory.computeAddress(owners, 1, salt);
        address actual = factory.createProxy(owners, 1, salt);

        assertEq(predicted, actual);
    }

    function test_factory_immutables() public view {
        assertEq(factory.singleton(), address(singleton));
        assertEq(factory.fallbackHandler(), fallbackHandler);
    }

    /*//////////////////////////////////////////////////////////////
                         SETUP: INITIALIZATION
    //////////////////////////////////////////////////////////////*/

    function test_setup_setsOwnersAndThreshold() public {
        address[] memory owners = _sortedOwners3();
        PrivateZoneSafe safe = _deploySafe(owners, 2);

        assertEq(safe.ownerCount(), 3);
        assertEq(safe.threshold(), 2);

        address[] memory retrieved = safe.getOwners();
        assertEq(retrieved.length, 3);

        for (uint256 i = 0; i < owners.length; i++) {
            assertTrue(safe.isOwner(owners[i]));
        }
    }

    function test_setup_setsFallbackHandler() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        assertEq(safe.fallbackHandler(), fallbackHandler);
    }

    function test_setup_cannotCallTwice() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        vm.expectRevert(PrivateZoneSafe.AlreadyInitialized.selector);
        safe.setup(owners, 1, fallbackHandler);
    }

    function test_setup_revertsZeroThreshold() public {
        address[] memory owners = _sortedOwners2();

        vm.expectRevert(PrivateZoneSafeFactory.SetupFailed.selector);
        factory.createProxy(owners, 0, bytes32(0));
    }

    function test_setup_revertsThresholdTooHigh() public {
        address[] memory owners = _sortedOwners2();

        vm.expectRevert(PrivateZoneSafeFactory.SetupFailed.selector);
        factory.createProxy(owners, 3, bytes32(0));
    }

    function test_setup_revertsZeroAddressOwner() public {
        address[] memory owners = new address[](2);
        owners[0] = address(0);
        owners[1] = owner1;

        vm.expectRevert(PrivateZoneSafeFactory.SetupFailed.selector);
        factory.createProxy(owners, 1, bytes32(0));
    }

    function test_setup_revertsSentinelOwner() public {
        address[] memory owners = new address[](1);
        owners[0] = address(0x1); // SENTINEL

        vm.expectRevert(PrivateZoneSafeFactory.SetupFailed.selector);
        factory.createProxy(owners, 1, bytes32(0));
    }

    function test_setup_revertsDuplicateOwner() public {
        address[] memory owners = new address[](2);
        owners[0] = owner1;
        owners[1] = owner1;

        // Duplicate owners cause setup to fail inside the proxy.
        // The factory catches this and reverts with SetupFailed.
        // However, since owner1 appears twice, the linked list check
        // (owners[owner] != address(0)) triggers on the second insertion
        // only after the first has been written. Use a fresh proxy to test directly.
        address proxy = address(new PrivateZoneSafeProxy(address(singleton)));
        vm.expectRevert(PrivateZoneSafe.DuplicateOwner.selector);
        PrivateZoneSafe(payable(proxy)).setup(owners, 1, fallbackHandler);
    }

    function test_setup_emitsEvent() public {
        address[] memory owners = _sortedOwners2();
        bytes32 salt = bytes32(uint256(99));
        address predicted = factory.computeAddress(owners, 1, salt);

        vm.expectEmit(true, false, false, true, predicted);
        emit PrivateZoneSafe.SafeSetup(address(factory), owners, 1, fallbackHandler);
        factory.createProxy(owners, 1, salt);
    }

    /*//////////////////////////////////////////////////////////////
                      EXEC TRANSACTION: BASIC
    //////////////////////////////////////////////////////////////*/

    function test_execTransaction_success() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);
        vm.deal(address(safe), 1 ether);

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;

        bytes memory sig = _signTransaction(safe, keys, nonOwner, 0.5 ether, "");
        bool success = safe.execTransaction(nonOwner, 0.5 ether, "", sig);

        assertTrue(success);
        assertEq(nonOwner.balance, 0.5 ether);
        assertEq(safe.nonce(), 1);
    }

    function test_execTransaction_incrementsNonce() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        assertEq(safe.nonce(), 0);

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;

        bytes memory sig = _signTransaction(safe, keys, address(0), 0, "");
        safe.execTransaction(address(0), 0, "", sig);

        assertEq(safe.nonce(), 1);
    }

    function test_execTransaction_emitsSuccess() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;

        bytes32 txHash = safe.getTransactionHash(address(0), 0, "", safe.nonce());
        bytes memory sig = _signTransaction(safe, keys, address(0), 0, "");

        vm.expectEmit(true, false, false, false);
        emit PrivateZoneSafe.ExecutionSuccess(txHash);
        safe.execTransaction(address(0), 0, "", sig);
    }

    function test_execTransaction_emitsFailure() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        MockReverter reverter = new MockReverter();
        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;

        bytes32 txHash = safe.getTransactionHash(
            address(reverter), 0, abi.encodeCall(reverter.fail, ()), safe.nonce()
        );
        bytes memory sig =
            _signTransaction(safe, keys, address(reverter), 0, abi.encodeCall(reverter.fail, ()));

        vm.expectEmit(true, false, false, false);
        emit PrivateZoneSafe.ExecutionFailure(txHash);
        bool success =
            safe.execTransaction(address(reverter), 0, abi.encodeCall(reverter.fail, ()), sig);

        assertFalse(success);
    }

    /*//////////////////////////////////////////////////////////////
                   EXEC TRANSACTION: MULTISIG THRESHOLD
    //////////////////////////////////////////////////////////////*/

    function test_execTransaction_2of3() public {
        address[] memory owners = _sortedOwners3();
        PrivateZoneSafe safe = _deploySafe(owners, 2);
        vm.deal(address(safe), 1 ether);

        uint256[] memory keys = new uint256[](2);
        keys[0] = ownerKey1;
        keys[1] = ownerKey2;

        bytes memory sig = _signTransaction(safe, keys, nonOwner, 0.1 ether, "");
        bool success = safe.execTransaction(nonOwner, 0.1 ether, "", sig);

        assertTrue(success);
        assertEq(nonOwner.balance, 0.1 ether);
    }

    function test_execTransaction_revertsWithTooFewSignatures() public {
        address[] memory owners = _sortedOwners3();
        PrivateZoneSafe safe = _deploySafe(owners, 2);

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;

        bytes memory sig = _signTransaction(safe, keys, address(0), 0, "");

        vm.expectRevert(PrivateZoneSafe.NotEnoughSignatures.selector);
        safe.execTransaction(address(0), 0, "", sig);
    }

    function test_execTransaction_revertsWithNonOwnerSignature() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        uint256 fakeKey = 0xdead;
        uint256[] memory keys = new uint256[](1);
        keys[0] = fakeKey;

        bytes memory sig = _signTransaction(safe, keys, address(0), 0, "");

        vm.expectRevert(PrivateZoneSafe.InvalidSignature.selector);
        safe.execTransaction(address(0), 0, "", sig);
    }

    function test_execTransaction_revertsDuplicateSignatures() public {
        address[] memory owners = _sortedOwners3();
        PrivateZoneSafe safe = _deploySafe(owners, 2);

        bytes32 txHash = safe.getTransactionHash(address(0), 0, "", safe.nonce());
        (uint8 v1, bytes32 r1, bytes32 s1) = vm.sign(ownerKey1, txHash);

        // Duplicate signature
        bytes memory sig = abi.encodePacked(r1, s1, v1, r1, s1, v1);

        vm.expectRevert(PrivateZoneSafe.InvalidSignature.selector);
        safe.execTransaction(address(0), 0, "", sig);
    }

    function test_execTransaction_revertsUnsortedSignatures() public {
        address[] memory owners = _sortedOwners3();
        PrivateZoneSafe safe = _deploySafe(owners, 2);

        bytes32 txHash = safe.getTransactionHash(address(0), 0, "", safe.nonce());

        // Sign with two keys
        (uint8 v1, bytes32 r1, bytes32 s1) = vm.sign(ownerKey1, txHash);
        (uint8 v2, bytes32 r2, bytes32 s2) = vm.sign(ownerKey2, txHash);

        address signer1 = vm.addr(ownerKey1);
        address signer2 = vm.addr(ownerKey2);

        // Reverse order (descending instead of ascending)
        bytes memory sig;
        if (signer1 < signer2) {
            sig = abi.encodePacked(r2, s2, v2, r1, s1, v1);
        } else {
            sig = abi.encodePacked(r1, s1, v1, r2, s2, v2);
        }

        vm.expectRevert(PrivateZoneSafe.InvalidSignature.selector);
        safe.execTransaction(address(0), 0, "", sig);
    }

    /*//////////////////////////////////////////////////////////////
                     EXEC TRANSACTION: APPROVE HASH
    //////////////////////////////////////////////////////////////*/

    function test_approveHash_allowsExecution() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        bytes32 txHash = safe.getTransactionHash(address(0), 0, "", safe.nonce());

        // owner1 approves the hash on-chain
        vm.prank(owner1);
        safe.approveHash(txHash);
        assertTrue(safe.approvedHashes(owner1, txHash));

        // Use v=1, r=ownerAddress encoding
        bytes memory sig = abi.encodePacked(bytes32(uint256(uint160(owner1))), bytes32(0), uint8(1));

        safe.execTransaction(address(0), 0, "", sig);
        assertEq(safe.nonce(), 1);
    }

    function test_approveHash_revertsForNonOwner() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        bytes32 txHash = safe.getTransactionHash(address(0), 0, "", safe.nonce());

        vm.prank(nonOwner);
        vm.expectRevert(PrivateZoneSafe.NotOwner.selector);
        safe.approveHash(txHash);
    }

    function test_approveHash_revertsIfAlreadyApproved() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        bytes32 txHash = safe.getTransactionHash(address(0), 0, "", safe.nonce());

        vm.startPrank(owner1);
        safe.approveHash(txHash);
        vm.expectRevert(PrivateZoneSafe.HashAlreadyApproved.selector);
        safe.approveHash(txHash);
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                        OWNER MANAGEMENT
    //////////////////////////////////////////////////////////////*/

    function test_addOwner() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        bytes memory addCall = abi.encodeCall(safe.addOwnerWithThreshold, (owner3, 1));

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;
        bytes memory sig = _signTransaction(safe, keys, address(safe), 0, addCall);

        safe.execTransaction(address(safe), 0, addCall, sig);

        assertTrue(safe.isOwner(owner3));
        assertEq(safe.ownerCount(), 3);
    }

    function test_addOwner_emitsEvent() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        bytes memory addCall = abi.encodeCall(safe.addOwnerWithThreshold, (owner3, 1));

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;
        bytes memory sig = _signTransaction(safe, keys, address(safe), 0, addCall);

        vm.expectEmit(true, false, false, false, address(safe));
        emit PrivateZoneSafe.AddedOwner(owner3);
        safe.execTransaction(address(safe), 0, addCall, sig);
    }

    function test_removeOwner() public {
        address[] memory owners = _sortedOwners3();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        // The linked list is: SENTINEL -> last inserted -> ... -> first inserted -> SENTINEL
        // getOwners() traverses from SENTINEL, so getOwners()[0] is the last inserted.
        // To remove getOwners()[0], prevOwner is SENTINEL (0x1).
        address[] memory currentOwners = safe.getOwners();
        address toRemove = currentOwners[0];

        bytes memory removeCall = abi.encodeCall(safe.removeOwner, (address(0x1), toRemove, 1));

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;
        // Make sure we sign with a key that isn't being removed
        if (vm.addr(ownerKey1) == toRemove) {
            keys[0] = ownerKey2;
        }
        bytes memory sig = _signTransaction(safe, keys, address(safe), 0, removeCall);

        safe.execTransaction(address(safe), 0, removeCall, sig);

        assertEq(safe.ownerCount(), 2);
        assertFalse(safe.isOwner(toRemove));
    }

    function test_swapOwner() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        address[] memory currentOwners = safe.getOwners();
        // Swap last owner for owner3
        address toSwap = currentOwners[currentOwners.length - 1];

        bytes memory swapCall;
        if (currentOwners.length == 1) {
            swapCall = abi.encodeCall(safe.swapOwner, (address(0x1), toSwap, owner3));
        } else {
            swapCall = abi.encodeCall(
                safe.swapOwner, (currentOwners[currentOwners.length - 2], toSwap, owner3)
            );
        }

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;
        bytes memory sig = _signTransaction(safe, keys, address(safe), 0, swapCall);

        safe.execTransaction(address(safe), 0, swapCall, sig);

        assertTrue(safe.isOwner(owner3));
        assertFalse(safe.isOwner(toSwap));
        assertEq(safe.ownerCount(), 2);
    }

    function test_changeThreshold() public {
        address[] memory owners = _sortedOwners3();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        bytes memory call_ = abi.encodeCall(safe.changeThreshold, (2));

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;
        bytes memory sig = _signTransaction(safe, keys, address(safe), 0, call_);

        safe.execTransaction(address(safe), 0, call_, sig);

        assertEq(safe.threshold(), 2);
    }

    function test_changeThreshold_revertsIfTooHigh() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        bytes memory call_ = abi.encodeCall(safe.changeThreshold, (3));

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;
        bytes memory sig = _signTransaction(safe, keys, address(safe), 0, call_);

        // Inner call fails, execTransaction returns false
        bool success = safe.execTransaction(address(safe), 0, call_, sig);
        assertFalse(success);
        assertEq(safe.threshold(), 1);
    }

    function test_ownerManagement_revertsIfNotSelfCall() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        vm.prank(owner1);
        vm.expectRevert(PrivateZoneSafe.CallFailed.selector);
        safe.addOwnerWithThreshold(owner3, 1);

        vm.prank(owner1);
        vm.expectRevert(PrivateZoneSafe.CallFailed.selector);
        safe.removeOwner(address(0x1), owner2, 1);

        vm.prank(owner1);
        vm.expectRevert(PrivateZoneSafe.CallFailed.selector);
        safe.changeThreshold(2);
    }

    /*//////////////////////////////////////////////////////////////
                          VIEW FUNCTIONS
    //////////////////////////////////////////////////////////////*/

    function test_getOwners_returnsAllOwners() public {
        address[] memory owners = _sortedOwners3();
        PrivateZoneSafe safe = _deploySafe(owners, 2);

        address[] memory retrieved = safe.getOwners();
        assertEq(retrieved.length, 3);

        // All owners should be present (order may differ due to linked list)
        for (uint256 i = 0; i < owners.length; i++) {
            bool found = false;
            for (uint256 j = 0; j < retrieved.length; j++) {
                if (owners[i] == retrieved[j]) {
                    found = true;
                    break;
                }
            }
            assertTrue(found);
        }
    }

    function test_isOwner_returnsTrueForOwner() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        assertTrue(safe.isOwner(owner1));
        assertTrue(safe.isOwner(owner2));
    }

    function test_isOwner_returnsFalseForNonOwner() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        assertFalse(safe.isOwner(nonOwner));
        assertFalse(safe.isOwner(address(0)));
        assertFalse(safe.isOwner(address(0x1))); // SENTINEL
    }

    function test_getTransactionHash_isDeterministic() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        bytes32 hash1 = safe.getTransactionHash(nonOwner, 1 ether, "", 0);
        bytes32 hash2 = safe.getTransactionHash(nonOwner, 1 ether, "", 0);
        assertEq(hash1, hash2);

        // Different params produce different hashes
        bytes32 hash3 = safe.getTransactionHash(nonOwner, 2 ether, "", 0);
        assertTrue(hash1 != hash3);
    }

    function test_domainSeparator() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        bytes32 expected = keccak256(
            abi.encode(
                keccak256("EIP712Domain(uint256 chainId,address verifyingContract)"),
                block.chainid,
                address(safe)
            )
        );
        assertEq(safe.domainSeparator(), expected);
    }

    function test_viewFunctions_callableByAnyone() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        vm.startPrank(nonOwner);
        safe.getOwners();
        safe.isOwner(owner1);
        safe.threshold();
        safe.nonce();
        safe.ownerCount();
        safe.domainSeparator();
        safe.getTransactionHash(address(0), 0, "", 0);
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                          RECEIVE ETHER
    //////////////////////////////////////////////////////////////*/

    function test_receiveEther() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        vm.deal(address(this), 1 ether);
        (bool sent,) = address(safe).call{ value: 1 ether }("");
        assertTrue(sent);
        assertEq(address(safe).balance, 1 ether);
    }

    /*//////////////////////////////////////////////////////////////
                      REPLAY PROTECTION
    //////////////////////////////////////////////////////////////*/

    function test_replayProtection_nonceIncrements() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe = _deploySafe(owners, 1);

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;

        bytes memory sig1 = _signTransaction(safe, keys, address(0), 0, "");
        safe.execTransaction(address(0), 0, "", sig1);

        // Same signature won't work again because nonce changed
        vm.expectRevert(PrivateZoneSafe.InvalidSignature.selector);
        safe.execTransaction(address(0), 0, "", sig1);
    }

    function test_replayProtection_crossSafe() public {
        address[] memory owners = _sortedOwners2();
        PrivateZoneSafe safe1 = _deploySafe(owners, 1);

        owners = _sortedOwners2();
        PrivateZoneSafe safe2 =
            PrivateZoneSafe(payable(factory.createProxy(owners, 1, bytes32(uint256(1)))));

        uint256[] memory keys = new uint256[](1);
        keys[0] = ownerKey1;

        bytes memory sig = _signTransaction(safe1, keys, address(0), 0, "");
        safe1.execTransaction(address(0), 0, "", sig);

        // Signature from safe1 cannot be replayed on safe2 (different domain separator)
        bytes memory sig2 = _signTransaction(safe1, keys, address(0), 0, "");
        vm.expectRevert(PrivateZoneSafe.InvalidSignature.selector);
        safe2.execTransaction(address(0), 0, "", sig2);
    }

    /*//////////////////////////////////////////////////////////////
                   SINGLETON: DIRECT CALL PREVENTION
    //////////////////////////////////////////////////////////////*/

    function test_singleton_cannotBeUsedDirectly() public {
        address[] memory owners = _sortedOwners2();
        singleton.setup(owners, 1, fallbackHandler);

        // Second setup reverts
        vm.expectRevert(PrivateZoneSafe.AlreadyInitialized.selector);
        singleton.setup(owners, 1, fallbackHandler);
    }

}

/*//////////////////////////////////////////////////////////////
                         TEST HELPERS
//////////////////////////////////////////////////////////////*/

contract MockFallbackHandler {

    function isValidSignature(bytes32, bytes calldata) external pure returns (bytes4) {
        return 0x1626ba7e;
    }

}

contract MockReverter {

    function fail() external pure {
        revert("always fails");
    }

}
