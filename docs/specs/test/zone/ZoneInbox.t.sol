// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { EncryptedDepositLib } from "../../src/zone/EncryptedDeposit.sol";
import {
    AES_GCM_DECRYPT,
    CHAUM_PEDERSEN_VERIFY,
    ChaumPedersenProof,
    DecryptionData,
    Deposit,
    DepositType,
    EncryptedDeposit,
    EncryptedDepositPayload,
    IAesGcmDecrypt,
    IChaumPedersenVerify,
    IZoneConfig,
    IZoneInbox,
    PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
    PORTAL_ENCRYPTION_KEYS_SLOT,
    QueuedDeposit
} from "../../src/zone/IZone.sol";
import { ZoneConfig } from "../../src/zone/ZoneConfig.sol";
import { ZoneInbox } from "../../src/zone/ZoneInbox.sol";
import { MockTempoState } from "./mocks/MockTempoState.sol";
import { MockZoneToken } from "./mocks/MockZoneToken.sol";
import { Test } from "forge-std/Test.sol";

/// @title ZoneInboxTest
/// @notice Tests for ZoneInbox covering edge cases
contract ZoneInboxTest is Test {

    ZoneConfig public config;
    ZoneInbox public inbox;
    MockZoneToken public zoneToken;
    MockTempoState public tempoState;

    address public sequencer = address(0x1);
    address public alice = address(0x200);
    address public bob = address(0x300);
    address public mockPortal = address(0x400);

    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 constant GENESIS_TEMPO_BLOCK_NUMBER = 1;

    function setUp() public {
        zoneToken = new MockZoneToken("Zone USD", "zUSD");
        tempoState =
            new MockTempoState(sequencer, GENESIS_TEMPO_BLOCK_HASH, GENESIS_TEMPO_BLOCK_NUMBER);
        config = new ZoneConfig(mockPortal, address(tempoState));
        tempoState.setMockStorageValue(
            mockPortal, bytes32(uint256(0)), bytes32(uint256(uint160(sequencer)))
        );
        inbox = new ZoneInbox(address(config), mockPortal, address(tempoState));

        zoneToken.setMinter(address(inbox), true);
    }

    function _wrapDeposits(Deposit[] memory deposits)
        internal
        pure
        returns (QueuedDeposit[] memory queued)
    {
        queued = new QueuedDeposit[](deposits.length);
        for (uint256 i = 0; i < deposits.length; i++) {
            queued[i] = QueuedDeposit({
                depositType: DepositType.Regular, depositData: abi.encode(deposits[i])
            });
        }
    }

    function _advanceTempo(Deposit[] memory deposits) internal {
        inbox.advanceTempo("", _wrapDeposits(deposits), new DecryptionData[](0));
    }

    /*//////////////////////////////////////////////////////////////
                          EMPTY DEPOSITS TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_emptyDepositsArray() public {
        // Set mock to return bytes32(0) for currentDepositQueueHash (empty queue)
        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, bytes32(0)
        );

        Deposit[] memory deposits = new Deposit[](0);

        vm.prank(sequencer);
        _advanceTempo(deposits);

        // State should remain at bytes32(0)
        assertEq(inbox.processedDepositQueueHash(), bytes32(0));
    }

    function test_advanceTempo_singleDeposit() public {
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            token: address(zoneToken),
            sender: alice,
            to: bob,
            amount: 1000e6,
            memo: bytes32("payment")
        });

        // Calculate expected hash
        bytes32 expectedHash = keccak256(abi.encode(DepositType.Regular, deposits[0], bytes32(0)));

        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash
        );

        vm.prank(sequencer);
        _advanceTempo(deposits);

        assertEq(inbox.processedDepositQueueHash(), expectedHash);
        assertEq(zoneToken.balanceOf(bob), 1000e6);
    }

    function test_advanceTempo_multipleDeposits() public {
        Deposit[] memory deposits = new Deposit[](3);
        deposits[0] = Deposit({
            token: address(zoneToken), sender: alice, to: alice, amount: 100e6, memo: bytes32("d1")
        });
        deposits[1] = Deposit({
            token: address(zoneToken), sender: bob, to: bob, amount: 200e6, memo: bytes32("d2")
        });
        deposits[2] = Deposit({
            token: address(zoneToken), sender: alice, to: bob, amount: 300e6, memo: bytes32("d3")
        });

        // Calculate expected hash chain
        bytes32 h0 = bytes32(0);
        bytes32 h1 = keccak256(abi.encode(DepositType.Regular, deposits[0], h0));
        bytes32 h2 = keccak256(abi.encode(DepositType.Regular, deposits[1], h1));
        bytes32 h3 = keccak256(abi.encode(DepositType.Regular, deposits[2], h2));

        tempoState.setMockStorageValue(mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, h3);

        vm.prank(sequencer);
        _advanceTempo(deposits);

        assertEq(inbox.processedDepositQueueHash(), h3);
        assertEq(zoneToken.balanceOf(alice), 100e6);
        assertEq(zoneToken.balanceOf(bob), 200e6 + 300e6);
    }

    /*//////////////////////////////////////////////////////////////
                    HASH CHAIN VALIDATION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_allowsHashMismatch() public {
        // Hash mismatch is now allowed on-chain — the proof validates ancestor contiguity
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            token: address(zoneToken),
            sender: alice,
            to: bob,
            amount: 1000e6,
            memo: bytes32("payment")
        });

        // Set a different hash (simulating more deposits pending on Tempo)
        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, keccak256("moreDepositsPending")
        );

        bytes32 expectedHash = keccak256(abi.encode(DepositType.Regular, deposits[0], bytes32(0)));

        vm.prank(sequencer);
        _advanceTempo(deposits);

        // Deposits are processed and state is updated
        assertEq(inbox.processedDepositQueueHash(), expectedHash);
        assertEq(zoneToken.balanceOf(bob), 1000e6);
    }

    function test_advanceTempo_partialProcessingAllowed() public {
        // Partial processing is now allowed — the proof validates ancestor contiguity
        Deposit[] memory allDeposits = new Deposit[](2);
        allDeposits[0] = Deposit({
            token: address(zoneToken), sender: alice, to: alice, amount: 100e6, memo: bytes32("d1")
        });
        allDeposits[1] = Deposit({
            token: address(zoneToken), sender: bob, to: bob, amount: 200e6, memo: bytes32("d2")
        });

        // Set hash to be for both deposits
        bytes32 h0 = bytes32(0);
        bytes32 h1 = keccak256(abi.encode(DepositType.Regular, allDeposits[0], h0));
        bytes32 h2 = keccak256(abi.encode(DepositType.Regular, allDeposits[1], h1));

        tempoState.setMockStorageValue(mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, h2);

        // Process only one deposit — should succeed (partial processing)
        Deposit[] memory oneDeposit = new Deposit[](1);
        oneDeposit[0] = allDeposits[0];

        vm.prank(sequencer);
        _advanceTempo(oneDeposit);

        // State updated to intermediate hash
        assertEq(inbox.processedDepositQueueHash(), h1);
        assertEq(zoneToken.balanceOf(alice), 100e6);

        // Process the second deposit to catch up
        Deposit[] memory secondDeposit = new Deposit[](1);
        secondDeposit[0] = allDeposits[1];

        vm.prank(sequencer);
        _advanceTempo(secondDeposit);

        assertEq(inbox.processedDepositQueueHash(), h2);
        assertEq(zoneToken.balanceOf(bob), 200e6);
    }

    /*//////////////////////////////////////////////////////////////
                         ACCESS CONTROL TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_onlySequencer() public {
        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, bytes32(0)
        );

        Deposit[] memory deposits = new Deposit[](0);

        // Random user should fail
        vm.prank(alice);
        vm.expectRevert(IZoneInbox.OnlySequencer.selector);
        _advanceTempo(deposits);

        // Sequencer should succeed
        vm.prank(sequencer);
        _advanceTempo(deposits);
    }

    /*//////////////////////////////////////////////////////////////
                    MAX DEPOSITS PER TEMPO BLOCK TESTS
    //////////////////////////////////////////////////////////////*/

    function test_setMaxDepositsPerTempoBlock_onlySequencer() public {
        vm.prank(alice);
        vm.expectRevert(IZoneInbox.OnlySequencer.selector);
        inbox.setMaxDepositsPerTempoBlock(10);

        vm.prank(sequencer);
        inbox.setMaxDepositsPerTempoBlock(10);
        assertEq(inbox.maxDepositsPerTempoBlock(), 10);
    }

    function test_setMaxDepositsPerTempoBlock_emitsEvent() public {
        vm.prank(sequencer);
        vm.expectEmit(false, false, false, true);
        emit IZoneInbox.MaxDepositsPerTempoBlockUpdated(5);
        inbox.setMaxDepositsPerTempoBlock(5);
    }

    function test_setMaxDepositsPerTempoBlock_zeroMeansUnlimited() public {
        vm.prank(sequencer);
        inbox.setMaxDepositsPerTempoBlock(10);
        assertEq(inbox.maxDepositsPerTempoBlock(), 10);

        vm.prank(sequencer);
        inbox.setMaxDepositsPerTempoBlock(0);
        assertEq(inbox.maxDepositsPerTempoBlock(), 0);
    }

    function test_advanceTempo_respectsMaxDepositsPerTempoBlock() public {
        vm.prank(sequencer);
        inbox.setMaxDepositsPerTempoBlock(1);

        Deposit[] memory deposits = new Deposit[](2);
        deposits[0] = Deposit({
            token: address(zoneToken), sender: alice, to: alice, amount: 100e6, memo: bytes32("d1")
        });
        deposits[1] = Deposit({
            token: address(zoneToken), sender: bob, to: bob, amount: 200e6, memo: bytes32("d2")
        });

        bytes32 h0 = bytes32(0);
        bytes32 h1 = keccak256(abi.encode(DepositType.Regular, deposits[0], h0));
        bytes32 h2 = keccak256(abi.encode(DepositType.Regular, deposits[1], h1));

        tempoState.setMockStorageValue(mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, h2);

        vm.prank(sequencer);
        vm.expectRevert(IZoneInbox.TooManyDeposits.selector);
        _advanceTempo(deposits);

        // Process one at a time should work
        Deposit[] memory oneDeposit = new Deposit[](1);
        oneDeposit[0] = deposits[0];

        vm.prank(sequencer);
        _advanceTempo(oneDeposit);
        assertEq(inbox.processedDepositQueueHash(), h1);
        assertEq(zoneToken.balanceOf(alice), 100e6);
    }

    function test_advanceTempo_unlimitedDepositsWhenCapIsZero() public {
        assertEq(inbox.maxDepositsPerTempoBlock(), 0);

        Deposit[] memory deposits = new Deposit[](3);
        deposits[0] = Deposit({
            token: address(zoneToken), sender: alice, to: alice, amount: 100e6, memo: bytes32("d1")
        });
        deposits[1] = Deposit({
            token: address(zoneToken), sender: bob, to: bob, amount: 200e6, memo: bytes32("d2")
        });
        deposits[2] = Deposit({
            token: address(zoneToken), sender: alice, to: bob, amount: 300e6, memo: bytes32("d3")
        });

        bytes32 h0 = bytes32(0);
        bytes32 h1 = keccak256(abi.encode(DepositType.Regular, deposits[0], h0));
        bytes32 h2 = keccak256(abi.encode(DepositType.Regular, deposits[1], h1));
        bytes32 h3 = keccak256(abi.encode(DepositType.Regular, deposits[2], h2));

        tempoState.setMockStorageValue(mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, h3);

        vm.prank(sequencer);
        _advanceTempo(deposits);

        assertEq(inbox.processedDepositQueueHash(), h3);
    }

    /*//////////////////////////////////////////////////////////////
                        INCREMENTAL PROCESSING TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_incrementalProcessing() public {
        // First batch of deposits
        Deposit[] memory batch1 = new Deposit[](2);
        batch1[0] = Deposit({
            token: address(zoneToken), sender: alice, to: alice, amount: 100e6, memo: bytes32("d1")
        });
        batch1[1] = Deposit({
            token: address(zoneToken), sender: bob, to: bob, amount: 200e6, memo: bytes32("d2")
        });

        bytes32 h0 = bytes32(0);
        bytes32 h1 = keccak256(abi.encode(DepositType.Regular, batch1[0], h0));
        bytes32 h2 = keccak256(abi.encode(DepositType.Regular, batch1[1], h1));

        tempoState.setMockStorageValue(mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, h2);

        vm.prank(sequencer);
        _advanceTempo(batch1);

        assertEq(inbox.processedDepositQueueHash(), h2);

        // Second batch of deposits
        Deposit[] memory batch2 = new Deposit[](1);
        batch2[0] = Deposit({
            token: address(zoneToken), sender: alice, to: bob, amount: 500e6, memo: bytes32("d3")
        });

        bytes32 h3 = keccak256(abi.encode(DepositType.Regular, batch2[0], h2));

        tempoState.setMockStorageValue(mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, h3);

        vm.prank(sequencer);
        _advanceTempo(batch2);

        assertEq(inbox.processedDepositQueueHash(), h3);
        assertEq(zoneToken.balanceOf(alice), 100e6);
        assertEq(zoneToken.balanceOf(bob), 200e6 + 500e6);
    }

    /*//////////////////////////////////////////////////////////////
                          EVENT EMISSION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_emitsTempoAdvancedEvent() public {
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            token: address(zoneToken),
            sender: alice,
            to: bob,
            amount: 1000e6,
            memo: bytes32("payment")
        });

        bytes32 expectedHash = keccak256(abi.encode(DepositType.Regular, deposits[0], bytes32(0)));

        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash
        );

        vm.prank(sequencer);
        vm.expectEmit(true, true, false, true);
        // After finalizeTempo, block number will be GENESIS + 1
        emit IZoneInbox.TempoAdvanced(
            keccak256(abi.encode(GENESIS_TEMPO_BLOCK_HASH, GENESIS_TEMPO_BLOCK_NUMBER + 1)),
            GENESIS_TEMPO_BLOCK_NUMBER + 1,
            1,
            expectedHash
        );
        _advanceTempo(deposits);
    }

    function test_advanceTempo_emitsDepositProcessedEvent() public {
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            token: address(zoneToken),
            sender: alice,
            to: bob,
            amount: 1000e6,
            memo: bytes32("payment")
        });

        bytes32 expectedHash = keccak256(abi.encode(DepositType.Regular, deposits[0], bytes32(0)));

        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash
        );

        vm.prank(sequencer);
        vm.expectEmit(true, true, true, true);
        emit IZoneInbox.DepositProcessed(
            expectedHash, alice, bob, address(zoneToken), 1000e6, bytes32("payment")
        );
        _advanceTempo(deposits);
    }

    /*//////////////////////////////////////////////////////////////
                         ZERO AMOUNT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_zeroAmountDeposit() public {
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            token: address(zoneToken), sender: alice, to: bob, amount: 0, memo: bytes32("empty")
        });

        bytes32 expectedHash = keccak256(abi.encode(DepositType.Regular, deposits[0], bytes32(0)));

        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash
        );

        vm.prank(sequencer);
        _advanceTempo(deposits);

        assertEq(inbox.processedDepositQueueHash(), expectedHash);
        assertEq(zoneToken.balanceOf(bob), 0);
    }

    /*//////////////////////////////////////////////////////////////
                        IMMUTABLE GETTERS TESTS
    //////////////////////////////////////////////////////////////*/

    function test_immutableGetters() public view {
        assertEq(address(inbox.config()), address(config));
        assertEq(inbox.tempoPortal(), mockPortal);
        assertEq(address(inbox.tempoState()), address(tempoState));
    }

    /*//////////////////////////////////////////////////////////////
                      LARGE DEPOSIT BATCH TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_manyDeposits() public {
        uint256 numDeposits = 50;
        Deposit[] memory deposits = new Deposit[](numDeposits);

        bytes32 currentHash = bytes32(0);
        for (uint256 i = 0; i < numDeposits; i++) {
            deposits[i] = Deposit({
                token: address(zoneToken),
                sender: alice,
                to: bob,
                amount: uint128(i + 1) * 1e6,
                memo: bytes32(i)
            });
            currentHash = keccak256(abi.encode(DepositType.Regular, deposits[i], currentHash));
        }

        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, currentHash
        );

        vm.prank(sequencer);
        _advanceTempo(deposits);

        assertEq(inbox.processedDepositQueueHash(), currentHash);

        // Calculate expected balance: sum of 1 + 2 + ... + 50 = 50 * 51 / 2 = 1275
        uint256 expectedBalance = (numDeposits * (numDeposits + 1) / 2) * 1e6;
        assertEq(zoneToken.balanceOf(bob), expectedBalance);
    }

    /*//////////////////////////////////////////////////////////////
                    ENCRYPTED DEPOSIT TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Set up encryption key mock storage for a given key index
    function _setupEncryptionKeyMock(uint256 keyIndex, bytes32 keyX, uint8 keyYParity) internal {
        uint256 base = uint256(keccak256(abi.encode(uint256(PORTAL_ENCRYPTION_KEYS_SLOT))));
        uint256 slotX = base + (keyIndex * 2);
        uint256 slotMeta = slotX + 1;
        tempoState.setMockStorageValue(mockPortal, bytes32(slotX), keyX);
        tempoState.setMockStorageValue(mockPortal, bytes32(slotMeta), bytes32(uint256(keyYParity)));
    }

    /// @notice Build an EncryptedDeposit and its QueuedDeposit wrapper
    function _makeEncryptedDeposit(
        address sender,
        uint128 amount,
        uint256 keyIndex
    )
        internal
        view
        returns (QueuedDeposit memory qd, EncryptedDeposit memory ed)
    {
        ed = EncryptedDeposit({
            token: address(zoneToken),
            sender: sender,
            amount: amount,
            keyIndex: keyIndex,
            encrypted: EncryptedDepositPayload({
                ephemeralPubkeyX: bytes32(uint256(0x1234)),
                ephemeralPubkeyYParity: 0x02,
                ciphertext: new bytes(64),
                nonce: bytes12(0),
                tag: bytes16(0)
            })
        });
        qd = QueuedDeposit({ depositType: DepositType.Encrypted, depositData: abi.encode(ed) });
    }

    /// @notice Set up precompile mocks for successful encrypted deposit processing
    function _setupPrecompileMocks(address recipient, bytes32 memo) internal {
        // Deploy dummy code so high-level Solidity calls pass extcodesize check
        vm.etch(CHAUM_PEDERSEN_VERIFY, hex"00");
        vm.etch(AES_GCM_DECRYPT, hex"00");

        // Mock Chaum-Pedersen to return valid
        vm.mockCall(
            CHAUM_PEDERSEN_VERIFY,
            abi.encodeWithSelector(IChaumPedersenVerify.verifyProof.selector),
            abi.encode(true)
        );

        // Mock AES-GCM to return expected plaintext
        bytes memory plaintext = EncryptedDepositLib.encodePlaintext(recipient, memo);
        vm.mockCall(
            AES_GCM_DECRYPT,
            abi.encodeWithSelector(IAesGcmDecrypt.decrypt.selector),
            abi.encode(plaintext, true)
        );
    }

    function test_advanceTempo_encryptedDeposit_success() public {
        address recipient = address(0x500);
        bytes32 memo = bytes32("secret memo");
        uint128 amount = 1000e6;

        // Set up encryption key in mock Tempo storage
        bytes32 seqKeyX = keccak256("sequencer-key-x");
        uint8 seqKeyYParity = 0x03;
        _setupEncryptionKeyMock(0, seqKeyX, seqKeyYParity);

        // Set up precompile mocks
        _setupPrecompileMocks(recipient, memo);

        // Build encrypted deposit
        (QueuedDeposit memory qd, EncryptedDeposit memory ed) =
            _makeEncryptedDeposit(alice, amount, 0);

        // Compute expected hash and set in mock storage
        bytes32 expectedHash = keccak256(abi.encode(DepositType.Encrypted, ed, bytes32(0)));
        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash
        );

        // Build deposits and decryptions arrays
        QueuedDeposit[] memory deposits = new QueuedDeposit[](1);
        deposits[0] = qd;

        DecryptionData[] memory decs = new DecryptionData[](1);
        decs[0] = DecryptionData({
            sharedSecret: bytes32(uint256(0xdeadbeef)),
            sharedSecretYParity: 0x02,
            to: recipient,
            memo: memo,
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs);

        // Verify minting to the decrypted recipient
        assertEq(zoneToken.balanceOf(recipient), amount);
        assertEq(inbox.processedDepositQueueHash(), expectedHash);
    }

    function test_advanceTempo_encryptedDeposit_decryptionFails() public {
        uint128 amount = 1000e6;

        // Set up encryption key
        _setupEncryptionKeyMock(0, keccak256("seq-key"), 0x03);

        // Deploy dummy code at precompile addresses
        vm.etch(CHAUM_PEDERSEN_VERIFY, hex"00");
        vm.etch(AES_GCM_DECRYPT, hex"00");

        // Mock CP to return valid
        vm.mockCall(
            CHAUM_PEDERSEN_VERIFY,
            abi.encodeWithSelector(IChaumPedersenVerify.verifyProof.selector),
            abi.encode(true)
        );

        // Mock AES-GCM to return FAILURE
        vm.mockCall(
            AES_GCM_DECRYPT,
            abi.encodeWithSelector(IAesGcmDecrypt.decrypt.selector),
            abi.encode(new bytes(0), false)
        );

        // Build encrypted deposit
        (QueuedDeposit memory qd, EncryptedDeposit memory ed) =
            _makeEncryptedDeposit(alice, amount, 0);

        bytes32 expectedHash = keccak256(abi.encode(DepositType.Encrypted, ed, bytes32(0)));
        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash
        );

        QueuedDeposit[] memory deposits = new QueuedDeposit[](1);
        deposits[0] = qd;

        DecryptionData[] memory decs = new DecryptionData[](1);
        decs[0] = DecryptionData({
            sharedSecret: bytes32(uint256(0xdeadbeef)),
            sharedSecretYParity: 0x02,
            to: address(0x500),
            memo: bytes32("memo"),
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs);

        // Funds should go to sender (alice), not the decrypted recipient
        assertEq(zoneToken.balanceOf(alice), amount);
        assertEq(zoneToken.balanceOf(address(0x500)), 0);
        assertEq(inbox.processedDepositQueueHash(), expectedHash);
    }

    function test_advanceTempo_mixedRegularAndEncryptedDeposits() public {
        address recipient = address(0x500);
        bytes32 encMemo = bytes32("encrypted memo");

        // Set up encryption key
        _setupEncryptionKeyMock(0, keccak256("seq-key"), 0x03);
        _setupPrecompileMocks(recipient, encMemo);

        // Build regular deposit
        Deposit memory d = Deposit({
            token: address(zoneToken), sender: alice, to: bob, amount: 100e6, memo: bytes32("d1")
        });
        QueuedDeposit memory qdRegular =
            QueuedDeposit({ depositType: DepositType.Regular, depositData: abi.encode(d) });

        // Build encrypted deposit
        (QueuedDeposit memory qdEnc, EncryptedDeposit memory ed) =
            _makeEncryptedDeposit(bob, 200e6, 0);

        // Compute expected hash chain: regular first, then encrypted
        bytes32 h1 = keccak256(abi.encode(DepositType.Regular, d, bytes32(0)));
        bytes32 h2 = keccak256(abi.encode(DepositType.Encrypted, ed, h1));

        tempoState.setMockStorageValue(mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, h2);

        QueuedDeposit[] memory deposits = new QueuedDeposit[](2);
        deposits[0] = qdRegular;
        deposits[1] = qdEnc;

        DecryptionData[] memory decs = new DecryptionData[](1);
        decs[0] = DecryptionData({
            sharedSecret: bytes32(uint256(0xabcd)),
            sharedSecretYParity: 0x02,
            to: recipient,
            memo: encMemo,
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs);

        // Regular deposit: bob gets 100e6
        // Encrypted deposit: recipient gets 200e6
        assertEq(zoneToken.balanceOf(bob), 100e6);
        assertEq(zoneToken.balanceOf(recipient), 200e6);
        assertEq(inbox.processedDepositQueueHash(), h2);
    }

    function test_advanceTempo_missingDecryptionData() public {
        // Set up encryption key
        _setupEncryptionKeyMock(0, keccak256("seq-key"), 0x03);

        // Build encrypted deposit but provide NO decryption data
        (QueuedDeposit memory qd,) = _makeEncryptedDeposit(alice, 1000e6, 0);

        // We need to set the current hash to something - doesn't matter since we expect revert
        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, keccak256("whatever")
        );

        QueuedDeposit[] memory deposits = new QueuedDeposit[](1);
        deposits[0] = qd;

        DecryptionData[] memory emptyDecs = new DecryptionData[](0);

        vm.prank(sequencer);
        vm.expectRevert(IZoneInbox.MissingDecryptionData.selector);
        inbox.advanceTempo("", deposits, emptyDecs);
    }

    function test_advanceTempo_extraDecryptionData() public {
        // Build a regular deposit only (no encrypted deposits)
        Deposit memory d = Deposit({
            token: address(zoneToken), sender: alice, to: bob, amount: 100e6, memo: bytes32("d1")
        });
        QueuedDeposit memory qd =
            QueuedDeposit({ depositType: DepositType.Regular, depositData: abi.encode(d) });

        bytes32 expectedHash = keccak256(abi.encode(DepositType.Regular, d, bytes32(0)));
        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash
        );

        QueuedDeposit[] memory deposits = new QueuedDeposit[](1);
        deposits[0] = qd;

        // Provide decryption data even though there are no encrypted deposits
        DecryptionData[] memory decs = new DecryptionData[](1);
        decs[0] = DecryptionData({
            sharedSecret: bytes32(uint256(1)),
            sharedSecretYParity: 0x02,
            to: address(0x500),
            memo: bytes32("memo"),
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });

        vm.prank(sequencer);
        vm.expectRevert(IZoneInbox.ExtraDecryptionData.selector);
        inbox.advanceTempo("", deposits, decs);
    }

    /*//////////////////////////////////////////////////////////////
                    ZONE CONFIG ENCRYPTION KEY TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Verify ZoneConfig.sequencerEncryptionKey() reads from the correct storage slot.
    /// @dev Regression test for the bug where ZoneConfig read the wrong slot
    ///      instead of the _encryptionKeys dynamic array at slot 6.
    function test_zoneConfig_sequencerEncryptionKey_readsCorrectSlot() public {
        bytes32 keyX = keccak256("config-test-key");
        uint8 keyYParity = 0x03;

        // Simulate the _encryptionKeys array at slot 6:
        // Set array length = 1
        uint256 arraySlot = uint256(PORTAL_ENCRYPTION_KEYS_SLOT);
        tempoState.setMockStorageValue(mockPortal, bytes32(arraySlot), bytes32(uint256(1)));

        // Set the key entry data at the derived slots
        uint256 base = uint256(keccak256(abi.encode(arraySlot)));
        tempoState.setMockStorageValue(mockPortal, bytes32(base), keyX);
        tempoState.setMockStorageValue(mockPortal, bytes32(base + 1), bytes32(uint256(keyYParity)));

        // Read via ZoneConfig — this should use the _encryptionKeys array slot
        (bytes32 readX, uint8 readYParity) = config.sequencerEncryptionKey();
        assertEq(readX, keyX, "ZoneConfig should read key x from encryption keys array");
        assertEq(
            readYParity, keyYParity, "ZoneConfig should read yParity from encryption keys array"
        );
    }

    /// @notice Verify ZoneConfig.sequencerEncryptionKey() returns the LAST key when multiple exist.
    function test_zoneConfig_sequencerEncryptionKey_returnsLatestKey() public {
        bytes32 keyX1 = keccak256("old-key");
        bytes32 keyX2 = keccak256("new-key");
        uint8 yParity2 = 0x02;

        // Simulate 2 entries in _encryptionKeys
        uint256 arraySlot = uint256(PORTAL_ENCRYPTION_KEYS_SLOT);
        tempoState.setMockStorageValue(mockPortal, bytes32(arraySlot), bytes32(uint256(2)));

        uint256 base = uint256(keccak256(abi.encode(arraySlot)));

        // Entry 0
        tempoState.setMockStorageValue(mockPortal, bytes32(base), keyX1);
        tempoState.setMockStorageValue(mockPortal, bytes32(base + 1), bytes32(uint256(0x03)));

        // Entry 1 (latest)
        tempoState.setMockStorageValue(mockPortal, bytes32(base + 2), keyX2);
        tempoState.setMockStorageValue(mockPortal, bytes32(base + 3), bytes32(uint256(yParity2)));

        (bytes32 readX, uint8 readYParity) = config.sequencerEncryptionKey();
        assertEq(readX, keyX2, "should return the latest key");
        assertEq(readYParity, yParity2, "should return the latest yParity");
    }

    /// @notice Verify ZoneConfig.sequencerEncryptionKey() reverts when no keys exist.
    function test_zoneConfig_sequencerEncryptionKey_revertsWhenEmpty() public {
        // Array length = 0 (default)
        tempoState.setMockStorageValue(mockPortal, PORTAL_ENCRYPTION_KEYS_SLOT, bytes32(uint256(0)));

        vm.expectRevert(IZoneConfig.NoEncryptionKeySet.selector);
        config.sequencerEncryptionKey();
    }

    /// @notice Verify ZoneConfig and ZoneInbox read from the same encryption key slot.
    /// @dev Both contracts import PORTAL_ENCRYPTION_KEYS_SLOT from IZone.sol and must agree on derived slot computation.
    function test_zoneConfig_and_zoneInbox_readSameEncryptionKey() public {
        bytes32 keyX = keccak256("shared-key-test");
        uint8 keyYParity = 0x02;

        // Set up encryption key mock (same as _setupEncryptionKeyMock)
        _setupEncryptionKeyMock(0, keyX, keyYParity);

        // Also set the array length (ZoneConfig needs this, ZoneInbox._readEncryptionKey doesn't)
        tempoState.setMockStorageValue(mockPortal, PORTAL_ENCRYPTION_KEYS_SLOT, bytes32(uint256(1)));

        // Read via ZoneConfig
        (bytes32 configX, uint8 configYParity) = config.sequencerEncryptionKey();

        // The values read by ZoneConfig must match what ZoneInbox._readEncryptionKey would get
        assertEq(configX, keyX, "ZoneConfig and ZoneInbox must agree on key X");
        assertEq(configYParity, keyYParity, "ZoneConfig and ZoneInbox must agree on yParity");
    }

    /*//////////////////////////////////////////////////////////////
                    ENCRYPTED DEPOSIT TESTS (continued)
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_encryptedDeposit_invalidProof() public {
        uint128 amount = 1000e6;

        // Set up encryption key
        _setupEncryptionKeyMock(0, keccak256("seq-key"), 0x03);

        // Deploy dummy code at precompile addresses
        vm.etch(CHAUM_PEDERSEN_VERIFY, hex"00");
        vm.etch(AES_GCM_DECRYPT, hex"00");

        // Mock CP to return INVALID
        vm.mockCall(
            CHAUM_PEDERSEN_VERIFY,
            abi.encodeWithSelector(IChaumPedersenVerify.verifyProof.selector),
            abi.encode(false)
        );

        // Build encrypted deposit
        (QueuedDeposit memory qd,) = _makeEncryptedDeposit(alice, amount, 0);

        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, keccak256("whatever")
        );

        QueuedDeposit[] memory deposits = new QueuedDeposit[](1);
        deposits[0] = qd;

        DecryptionData[] memory decs = new DecryptionData[](1);
        decs[0] = DecryptionData({
            sharedSecret: bytes32(uint256(0xbad)),
            sharedSecretYParity: 0x02,
            to: address(0x500),
            memo: bytes32("memo"),
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });

        vm.prank(sequencer);
        vm.expectRevert(IZoneInbox.InvalidSharedSecretProof.selector);
        inbox.advanceTempo("", deposits, decs);
    }

    /*//////////////////////////////////////////////////////////////
                PLAINTEXT LENGTH VALIDATION TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Helper: set up an encrypted deposit flow where AES-GCM returns a specific plaintext
    function _setupEncryptedDepositWithPlaintext(
        bytes memory mockPlaintext,
        bool aesValid,
        address recipient,
        bytes32 memo
    )
        internal
        returns (QueuedDeposit[] memory deposits, DecryptionData[] memory decs)
    {
        uint128 amount = 1000e6;

        // Set up encryption key
        _setupEncryptionKeyMock(0, keccak256("seq-key"), 0x03);

        // Deploy dummy code at precompile addresses
        vm.etch(CHAUM_PEDERSEN_VERIFY, hex"00");
        vm.etch(AES_GCM_DECRYPT, hex"00");

        // Mock CP to return valid
        vm.mockCall(
            CHAUM_PEDERSEN_VERIFY,
            abi.encodeWithSelector(IChaumPedersenVerify.verifyProof.selector),
            abi.encode(true)
        );

        // Mock AES-GCM to return the specified plaintext
        vm.mockCall(
            AES_GCM_DECRYPT,
            abi.encodeWithSelector(IAesGcmDecrypt.decrypt.selector),
            abi.encode(mockPlaintext, aesValid)
        );

        // Build encrypted deposit
        (QueuedDeposit memory qd, EncryptedDeposit memory ed) =
            _makeEncryptedDeposit(alice, amount, 0);

        bytes32 expectedHash = keccak256(abi.encode(DepositType.Encrypted, ed, bytes32(0)));
        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash
        );

        deposits = new QueuedDeposit[](1);
        deposits[0] = qd;

        decs = new DecryptionData[](1);
        decs[0] = DecryptionData({
            sharedSecret: bytes32(uint256(0xdeadbeef)),
            sharedSecretYParity: 0x02,
            to: recipient,
            memo: memo,
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });
    }

    /// @notice Verify that a too-short plaintext (52 bytes) causes the deposit to bounce
    /// @dev This was the old boundary that used to pass (>= 52). Now requires exactly 64.
    function test_advanceTempo_encryptedDeposit_plaintextTooShort_bounces() public {
        address recipient = address(0x500);
        bytes32 memo = bytes32("secret memo");

        // Create a 52-byte plaintext (the old minimum that used to be accepted)
        bytes memory shortPlaintext = new bytes(52);
        // Write address and memo into the first 52 bytes (same layout as encodePlaintext but truncated)
        assembly {
            mstore(add(shortPlaintext, 32), shl(96, recipient))
            mstore(add(shortPlaintext, 52), memo)
        }

        (QueuedDeposit[] memory deposits, DecryptionData[] memory decs) =
            _setupEncryptedDepositWithPlaintext(shortPlaintext, true, recipient, memo);

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs);

        // Deposit should bounce to sender (alice) because plaintext length != 64
        assertEq(zoneToken.balanceOf(alice), 1000e6, "sender should receive bounced funds");
        assertEq(zoneToken.balanceOf(recipient), 0, "recipient should get nothing");
    }

    /// @notice Verify that a too-long plaintext (65 bytes) causes the deposit to bounce
    function test_advanceTempo_encryptedDeposit_plaintextTooLong_bounces() public {
        address recipient = address(0x500);
        bytes32 memo = bytes32("secret memo");

        // Create a 65-byte plaintext (one byte too many)
        bytes memory longPlaintext = new bytes(65);
        assembly {
            mstore(add(longPlaintext, 32), shl(96, recipient))
            mstore(add(longPlaintext, 52), memo)
        }

        (QueuedDeposit[] memory deposits, DecryptionData[] memory decs) =
            _setupEncryptedDepositWithPlaintext(longPlaintext, true, recipient, memo);

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs);

        // Deposit should bounce to sender
        assertEq(zoneToken.balanceOf(alice), 1000e6, "sender should receive bounced funds");
        assertEq(zoneToken.balanceOf(recipient), 0, "recipient should get nothing");
    }

    /// @notice Verify that an empty plaintext (0 bytes) causes the deposit to bounce
    function test_advanceTempo_encryptedDeposit_plaintextEmpty_bounces() public {
        address recipient = address(0x500);
        bytes32 memo = bytes32("secret memo");

        bytes memory emptyPlaintext = new bytes(0);

        (QueuedDeposit[] memory deposits, DecryptionData[] memory decs) =
            _setupEncryptedDepositWithPlaintext(emptyPlaintext, true, recipient, memo);

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs);

        // Deposit should bounce to sender
        assertEq(zoneToken.balanceOf(alice), 1000e6, "sender should receive bounced funds");
        assertEq(zoneToken.balanceOf(recipient), 0, "recipient should get nothing");
    }

    /// @notice Verify that exactly 64-byte plaintext with correct data succeeds
    function test_advanceTempo_encryptedDeposit_plaintextExact64_succeeds() public {
        address recipient = address(0x500);
        bytes32 memo = bytes32("secret memo");

        // Use the canonical encodePlaintext which produces exactly 64 bytes
        bytes memory correctPlaintext = EncryptedDepositLib.encodePlaintext(recipient, memo);

        (QueuedDeposit[] memory deposits, DecryptionData[] memory decs) =
            _setupEncryptedDepositWithPlaintext(correctPlaintext, true, recipient, memo);

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs);

        // Deposit should succeed — minted to the decrypted recipient
        assertEq(zoneToken.balanceOf(recipient), 1000e6, "recipient should receive funds");
        assertEq(zoneToken.balanceOf(alice), 0, "sender should get nothing (successful deposit)");
    }

}
