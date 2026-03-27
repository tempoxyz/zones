// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { DepositQueueLib } from "../../src/zone/DepositQueueLib.sol";
import {
    ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE,
    EncryptedDepositLib
} from "../../src/zone/EncryptedDeposit.sol";
import {
    Deposit,
    DepositType,
    EncryptedDeposit,
    EncryptedDepositPayload
} from "../../src/zone/IZone.sol";
import { Test } from "forge-std/Test.sol";

/// @notice External wrapper to test EncryptedDepositLib.decodePlaintext (which is internal)
contract PlaintextDecoder {

    function decode(bytes memory plaintext) external pure returns (address to, bytes32 memo) {
        return EncryptedDepositLib.decodePlaintext(plaintext);
    }

}

/// @title DepositQueueLibTest
/// @notice Direct tests for DepositQueueLib functionality
contract DepositQueueLibTest is Test {

    address public alice = address(0x200);
    address public bob = address(0x300);

    /*//////////////////////////////////////////////////////////////
                            ENQUEUE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_enqueue_singleDeposit() public pure {
        Deposit memory d = Deposit({
            token: address(0x1000),
            sender: address(0x200),
            to: address(0x300),
            amount: 100e6,
            memo: bytes32("memo"),
            bouncebackRecipient: address(0)
        });

        bytes32 newHash = DepositQueueLib.enqueue(bytes32(0), d);

        bytes32 expectedHash = keccak256(abi.encode(DepositType.Regular, d, bytes32(0)));
        assertEq(newHash, expectedHash);
    }

    function test_enqueue_multipleDeposits() public pure {
        Deposit memory d1 = Deposit({
            token: address(0x1000),
            sender: address(0x200),
            to: address(0x300),
            amount: 100e6,
            memo: bytes32("d1"),
            bouncebackRecipient: address(0)
        });
        Deposit memory d2 = Deposit({
            token: address(0x1000),
            sender: address(0x300),
            to: address(0x200),
            amount: 200e6,
            memo: bytes32("d2"),
            bouncebackRecipient: address(0)
        });
        Deposit memory d3 = Deposit({
            token: address(0x1000),
            sender: address(0x200),
            to: address(0x200),
            amount: 300e6,
            memo: bytes32("d3"),
            bouncebackRecipient: address(0)
        });

        bytes32 h1 = DepositQueueLib.enqueue(bytes32(0), d1);
        bytes32 h2 = DepositQueueLib.enqueue(h1, d2);
        bytes32 h3 = DepositQueueLib.enqueue(h2, d3);

        // Verify hash chain structure
        bytes32 expected1 = keccak256(abi.encode(DepositType.Regular, d1, bytes32(0)));
        bytes32 expected2 = keccak256(abi.encode(DepositType.Regular, d2, expected1));
        bytes32 expected3 = keccak256(abi.encode(DepositType.Regular, d3, expected2));

        assertEq(h1, expected1);
        assertEq(h2, expected2);
        assertEq(h3, expected3);
    }

    function test_enqueue_hashChainStructure() public pure {
        // Verify that newer deposits wrap older ones
        Deposit memory d1 = Deposit({
            token: address(0x1000),
            sender: address(0x200),
            to: address(0x300),
            amount: 100e6,
            memo: bytes32("first"),
            bouncebackRecipient: address(0)
        });
        Deposit memory d2 = Deposit({
            token: address(0x1000),
            sender: address(0x300),
            to: address(0x200),
            amount: 200e6,
            memo: bytes32("second"),
            bouncebackRecipient: address(0)
        });

        bytes32 h1 = DepositQueueLib.enqueue(bytes32(0), d1);
        bytes32 h2 = DepositQueueLib.enqueue(h1, d2);

        // h2 should wrap h1
        assertEq(h2, keccak256(abi.encode(DepositType.Regular, d2, h1)));
    }

    function test_enqueue_emptyToEmpty() public pure {
        // An empty deposit struct should still produce a valid hash
        Deposit memory d = Deposit({
            token: address(0x1000),
            sender: address(0),
            to: address(0),
            amount: 0,
            memo: bytes32(0),
            bouncebackRecipient: address(0)
        });

        bytes32 h = DepositQueueLib.enqueue(bytes32(0), d);
        bytes32 expected = keccak256(abi.encode(DepositType.Regular, d, bytes32(0)));
        assertEq(h, expected);
        assertTrue(h != bytes32(0)); // Hash of something is non-zero
    }

    function test_enqueue_differentInputsProduceDifferentHashes() public pure {
        Deposit memory d1 = Deposit({
            token: address(0x1000),
            sender: address(0x200),
            to: address(0x300),
            amount: 100e6,
            memo: bytes32("memo1"),
            bouncebackRecipient: address(0)
        });
        Deposit memory d2 = Deposit({
            token: address(0x1000),
            sender: address(0x200),
            to: address(0x300),
            amount: 100e6,
            memo: bytes32("memo2"), // Only memo differs
            bouncebackRecipient: address(0)
        });

        bytes32 h1 = DepositQueueLib.enqueue(bytes32(0), d1);
        bytes32 h2 = DepositQueueLib.enqueue(bytes32(0), d2);

        assertTrue(h1 != h2);
    }

    function test_enqueue_sameDepositDifferentPrevHashProducesDifferentResult() public pure {
        Deposit memory d = Deposit({
            token: address(0x1000),
            sender: address(0x200),
            to: address(0x300),
            amount: 100e6,
            memo: bytes32("memo"),
            bouncebackRecipient: address(0)
        });

        bytes32 h1 = DepositQueueLib.enqueue(bytes32(0), d);
        bytes32 h2 = DepositQueueLib.enqueue(bytes32(uint256(1)), d);

        assertTrue(h1 != h2);
    }

    /*//////////////////////////////////////////////////////////////
                    ENQUEUE ENCRYPTED TESTS
    //////////////////////////////////////////////////////////////*/

    function test_enqueueEncrypted_singleDeposit() public pure {
        EncryptedDeposit memory ed = EncryptedDeposit({
            token: address(0x1000),
            sender: address(0x200),
            amount: 100e6,
            keyIndex: 0,
            encrypted: EncryptedDepositPayload({
                ephemeralPubkeyX: bytes32(uint256(1)),
                ephemeralPubkeyYParity: 0x02,
                ciphertext: new bytes(64),
                nonce: bytes12(0),
                tag: bytes16(0)
            }),
            bouncebackRecipient: address(0)
        });

        bytes32 newHash = DepositQueueLib.enqueueEncrypted(bytes32(0), ed);
        bytes32 expectedHash = keccak256(abi.encode(DepositType.Encrypted, ed, bytes32(0)));
        assertEq(newHash, expectedHash);
    }

    function test_enqueueEncrypted_mixedQueue() public pure {
        Deposit memory d1 = Deposit({
            token: address(0x1000),
            sender: address(0x200),
            to: address(0x300),
            amount: 100e6,
            memo: bytes32("d1"),
            bouncebackRecipient: address(0)
        });

        EncryptedDeposit memory ed = EncryptedDeposit({
            token: address(0x1000),
            sender: address(0x300),
            amount: 200e6,
            keyIndex: 0,
            encrypted: EncryptedDepositPayload({
                ephemeralPubkeyX: bytes32(uint256(1)),
                ephemeralPubkeyYParity: 0x02,
                ciphertext: new bytes(64),
                nonce: bytes12(0),
                tag: bytes16(0)
            }),
            bouncebackRecipient: address(0)
        });

        Deposit memory d2 = Deposit({
            token: address(0x1000),
            sender: address(0x200),
            to: address(0x200),
            amount: 300e6,
            memo: bytes32("d3"),
            bouncebackRecipient: address(0)
        });

        bytes32 h1 = DepositQueueLib.enqueue(bytes32(0), d1);
        bytes32 h2 = DepositQueueLib.enqueueEncrypted(h1, ed);
        bytes32 h3 = DepositQueueLib.enqueue(h2, d2);

        bytes32 expected1 = keccak256(abi.encode(DepositType.Regular, d1, bytes32(0)));
        bytes32 expected2 = keccak256(abi.encode(DepositType.Encrypted, ed, expected1));
        bytes32 expected3 = keccak256(abi.encode(DepositType.Regular, d2, expected2));

        assertEq(h1, expected1);
        assertEq(h2, expected2);
        assertEq(h3, expected3);
    }

    function test_enqueue_typeDiscriminatorPreventsCollision() public pure {
        // Same sender/amount but different type discriminators produce different hashes
        Deposit memory d = Deposit({
            token: address(0x1000),
            sender: address(0x200),
            to: address(0x300),
            amount: 100e6,
            memo: bytes32("memo"),
            bouncebackRecipient: address(0)
        });

        EncryptedDeposit memory ed = EncryptedDeposit({
            token: address(0x1000),
            sender: address(0x200),
            amount: 100e6,
            keyIndex: 0,
            encrypted: EncryptedDepositPayload({
                ephemeralPubkeyX: bytes32(0),
                ephemeralPubkeyYParity: 0,
                ciphertext: "",
                nonce: bytes12(0),
                tag: bytes16(0)
            }),
            bouncebackRecipient: address(0)
        });

        bytes32 regularHash = DepositQueueLib.enqueue(bytes32(0), d);
        bytes32 encryptedHash = DepositQueueLib.enqueueEncrypted(bytes32(0), ed);

        assertTrue(regularHash != encryptedHash);
    }

    /*//////////////////////////////////////////////////////////////
                    PLAINTEXT ENCODE / DECODE TESTS
    //////////////////////////////////////////////////////////////*/

    PlaintextDecoder internal decoder;

    function _ensureDecoder() internal {
        if (address(decoder) == address(0)) {
            decoder = new PlaintextDecoder();
        }
    }

    /// @notice Round-trip: encodePlaintext → decodePlaintext recovers original values
    function test_plaintext_roundTrip() public {
        _ensureDecoder();
        address to = address(0x1234567890AbcdEF1234567890aBcdef12345678);
        bytes32 memo = bytes32("hello world");

        bytes memory encoded = EncryptedDepositLib.encodePlaintext(to, memo);
        assertEq(encoded.length, ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE, "encoded should be 64 bytes");

        (address decodedTo, bytes32 decodedMemo) = decoder.decode(encoded);
        assertEq(decodedTo, to, "decoded address mismatch");
        assertEq(decodedMemo, memo, "decoded memo mismatch");
    }

    /// @notice decodePlaintext rejects plaintext shorter than 64 bytes (e.g. 52 bytes)
    /// @dev Previously the check was >= 52, allowing out-of-bounds assembly reads
    function test_plaintext_rejectsTooShort() public {
        _ensureDecoder();
        bytes memory short = new bytes(52);
        vm.expectRevert(
            abi.encodeWithSelector(
                EncryptedDepositLib.InvalidPlaintextLength.selector,
                uint256(52),
                ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE
            )
        );
        decoder.decode(short);
    }

    /// @notice decodePlaintext rejects empty plaintext
    function test_plaintext_rejectsEmpty() public {
        _ensureDecoder();
        bytes memory empty = new bytes(0);
        vm.expectRevert(
            abi.encodeWithSelector(
                EncryptedDepositLib.InvalidPlaintextLength.selector,
                uint256(0),
                ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE
            )
        );
        decoder.decode(empty);
    }

    /// @notice decodePlaintext rejects plaintext longer than 64 bytes
    /// @dev Trailing bytes beyond 64 were previously silently ignored
    function test_plaintext_rejectsTooLong() public {
        _ensureDecoder();
        bytes memory long = new bytes(65);
        vm.expectRevert(
            abi.encodeWithSelector(
                EncryptedDepositLib.InvalidPlaintextLength.selector,
                uint256(65),
                ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE
            )
        );
        decoder.decode(long);
    }

    /// @notice decodePlaintext rejects 63-byte plaintext (off-by-one boundary)
    function test_plaintext_rejects63Bytes() public {
        _ensureDecoder();
        bytes memory almostRight = new bytes(63);
        vm.expectRevert(
            abi.encodeWithSelector(
                EncryptedDepositLib.InvalidPlaintextLength.selector,
                uint256(63),
                ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE
            )
        );
        decoder.decode(almostRight);
    }

    /// @notice decodePlaintext accepts exactly 64 bytes (boundary test)
    function test_plaintext_acceptsExact64Bytes() public {
        _ensureDecoder();
        bytes memory exact = new bytes(64);
        (address to, bytes32 memo) = decoder.decode(exact);
        assertEq(to, address(0), "zero-filled plaintext should decode to zero address");
        assertEq(memo, bytes32(0), "zero-filled plaintext should decode to zero memo");
    }

    /// @notice Fuzz test: decodePlaintext always reverts for length != 64
    function testFuzz_plaintext_rejectsWrongLength(uint256 len) public {
        _ensureDecoder();
        vm.assume(len != ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE);
        vm.assume(len <= 256); // keep allocation reasonable
        bytes memory data = new bytes(len);
        vm.expectRevert(
            abi.encodeWithSelector(
                EncryptedDepositLib.InvalidPlaintextLength.selector,
                len,
                ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE
            )
        );
        decoder.decode(data);
    }

}
