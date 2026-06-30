// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    AES_GCM_DECRYPT,
    CHAUM_PEDERSEN_VERIFY,
    ChaumPedersenProof,
    DecryptionData,
    Deposit,
    DepositType,
    EnabledToken,
    EncryptedDeposit,
    EncryptedDepositPayload,
    IAesGcmDecrypt,
    IChaumPedersenVerify,
    ITIP20ZoneFactory,
    IZoneConfig,
    IZoneInbox,
    PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
    PORTAL_ENCRYPTION_KEYS_SLOT,
    QueuedDeposit,
    TIP20_FACTORY_ADDRESS,
    ZONE_OUTBOX
} from "../../src/interfaces/IZone.sol";
import { EncryptedDepositLib } from "../../src/libraries/EncryptedDeposit.sol";
import { ZoneConfig } from "../../src/predeploys/ZoneConfig.sol";
import { ZoneInbox } from "../../src/predeploys/ZoneInbox.sol";
import { MockTempoState } from "../mocks/MockTempoState.sol";
import { MockZoneToken } from "../mocks/MockZoneToken.sol";
import { Test } from "forge-std/Test.sol";

/// @dev Exposes ZoneInbox's internal helpers for direct unit testing. The encrypted-deposit
///      suite reaches them only through the mocked AES-GCM precompile, so their outputs are
///      otherwise unobserved. `_hmacSha256` only uses the SHA256 precompile (collaborator
///      addresses irrelevant); `_readEncryptionKey` reads portal storage via TempoState.
contract ZoneInboxHarness is ZoneInbox {

    constructor(address portal, address state) ZoneInbox(address(0), portal, state) { }

    function hmacSha256(bytes memory key, bytes memory message) external view returns (bytes32) {
        return _hmacSha256(key, message);
    }

    function readEncryptionKey(uint256 keyIndex) external view returns (bytes32 x, uint8 yParity) {
        return _readEncryptionKey(keyIndex);
    }

}

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
        vm.etch(ZONE_OUTBOX, hex"00");

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
                depositType: DepositType.Regular,
                depositData: abi.encode(deposits[i]),
                rejected: false
            });
        }
    }

    function _advanceTempo(Deposit[] memory deposits) internal {
        inbox.advanceTempo(
            "", _wrapDeposits(deposits), new DecryptionData[](0), new EnabledToken[](0)
        );
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
            bouncebackRecipient: bob,
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
            token: address(zoneToken),
            sender: alice,
            to: alice,
            amount: 100e6,
            bouncebackRecipient: alice,
            memo: bytes32("d1")
        });
        deposits[1] = Deposit({
            token: address(zoneToken),
            sender: bob,
            to: bob,
            amount: 200e6,
            bouncebackRecipient: bob,
            memo: bytes32("d2")
        });
        deposits[2] = Deposit({
            token: address(zoneToken),
            sender: alice,
            to: bob,
            amount: 300e6,
            bouncebackRecipient: bob,
            memo: bytes32("d3")
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
            bouncebackRecipient: bob,
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
            token: address(zoneToken),
            sender: alice,
            to: alice,
            amount: 100e6,
            bouncebackRecipient: alice,
            memo: bytes32("d1")
        });
        allDeposits[1] = Deposit({
            token: address(zoneToken),
            sender: bob,
            to: bob,
            amount: 200e6,
            bouncebackRecipient: bob,
            memo: bytes32("d2")
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
                        INCREMENTAL PROCESSING TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_incrementalProcessing() public {
        // First batch of deposits
        Deposit[] memory batch1 = new Deposit[](2);
        batch1[0] = Deposit({
            token: address(zoneToken),
            sender: alice,
            to: alice,
            amount: 100e6,
            bouncebackRecipient: alice,
            memo: bytes32("d1")
        });
        batch1[1] = Deposit({
            token: address(zoneToken),
            sender: bob,
            to: bob,
            amount: 200e6,
            bouncebackRecipient: bob,
            memo: bytes32("d2")
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
            token: address(zoneToken),
            sender: alice,
            to: bob,
            amount: 500e6,
            bouncebackRecipient: bob,
            memo: bytes32("d3")
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
            bouncebackRecipient: bob,
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
            expectedHash,
            1
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
            bouncebackRecipient: bob,
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
            token: address(zoneToken),
            sender: alice,
            to: bob,
            amount: 0,
            bouncebackRecipient: bob,
            memo: bytes32("empty")
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
                bouncebackRecipient: bob,
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

    function _rep(bytes1 b, uint256 n) internal pure returns (bytes memory out) {
        out = new bytes(n);
        for (uint256 i = 0; i < n; i++) {
            out[i] = b;
        }
    }

    /*//////////////////////////////////////////////////////////////
                      INTERNAL HELPER UNIT TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Known-answer tests for the internal word-level HMAC-SHA256.
    /// @dev The encrypted-deposit suite mocks the AES-GCM precompile, so the HMAC output is
    ///      never observed there. These golden vectors (RFC 4231 cases 1-2 plus offline-computed
    ///      cases) exercise every key-length branch: < 32 bytes (first-word masking), == 32,
    ///      32-64 (second-word masking), == 64, and > 64 (key is hashed first).
    function test_hmac_knownAnswerVectors() public {
        ZoneInboxHarness h = new ZoneInboxHarness(mockPortal, address(tempoState));

        assertEq(
            h.hmacSha256(hex"4a656665", "what do ya want for nothing?"),
            0x5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843
        );
        assertEq(
            h.hmacSha256(_rep(0x0b, 20), "Hi There"),
            0xb0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7
        );
        assertEq(
            h.hmacSha256(_rep(0xaa, 32), "msg-32byte-key"),
            0xf7a62f1ceb146c3e9f1562ed8133a36fb13c7f4c3e2ad8e3d5df5ebbd72e520e
        );
        assertEq(
            h.hmacSha256(_rep(0xbb, 48), "msg-48byte-key"),
            0x827ae58dc9bf44ebb5ee7a2375b84a947400c0833665104c46d2879a4c91cf30
        );
        assertEq(
            h.hmacSha256(_rep(0xcc, 63), "msg-63byte-key"),
            0xb2b5e90568a216eb95fe94169e69fc4a18897e15e0b5922d41c5d5183c7c8afe
        );
        assertEq(
            h.hmacSha256(_rep(0xdd, 64), "msg-64byte-key"),
            0x84ff5b758d4d9e4eebc0a4f611e464a1afd7845c0fd0cb2b517a930faeb2ddaa
        );
        assertEq(
            h.hmacSha256(_rep(0xee, 65), "msg-65byte-key"),
            0x7265d8f20eba414e2ca620c3135b691818b2864ebfa513c801682e32fabc2884
        );
    }

    /// @notice Reads the encryption key at a high index to exercise the per-entry slot math.
    /// @dev base + index*2 for x and +1 for the meta word; a large activationBlock packed
    ///      above the y-parity ensures the low-byte parity extraction is exercised correctly.
    ///      Index 3 (not 0/1) exposes index*2 vs index/2/shift mutants.
    function test_readEncryptionKey_highIndex_readsCorrectSlots() public {
        ZoneInboxHarness h = new ZoneInboxHarness(mockPortal, address(tempoState));

        uint256 base = uint256(keccak256(abi.encode(uint256(PORTAL_ENCRYPTION_KEYS_SLOT))));
        uint256 slotX = base + (3 * 2);
        bytes32 keyX = keccak256("key-at-index-3");
        bytes32 meta = bytes32(uint256(0x02) | (uint256(123_456) << 8));
        tempoState.setMockStorageValue(mockPortal, bytes32(slotX), keyX);
        tempoState.setMockStorageValue(mockPortal, bytes32(slotX + 1), meta);

        (bytes32 readX, uint8 readYParity) = h.readEncryptionKey(3);
        assertEq(readX, keyX);
        assertEq(readYParity, 0x02);
    }

    /// @notice An empty x slot reverts even when the meta slot is populated, proving x is read
    ///         from base + index*2 (not the meta slot at +1).
    function test_readEncryptionKey_emptyXSlot_reverts() public {
        ZoneInboxHarness h = new ZoneInboxHarness(mockPortal, address(tempoState));

        uint256 base = uint256(keccak256(abi.encode(uint256(PORTAL_ENCRYPTION_KEYS_SLOT))));
        uint256 slotX = base + (2 * 2);
        tempoState.setMockStorageValue(mockPortal, bytes32(slotX + 1), bytes32(uint256(0x03)));

        vm.expectRevert();
        h.readEncryptionKey(2);
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
            bouncebackRecipient: sender,
            keyIndex: keyIndex,
            encrypted: EncryptedDepositPayload({
                ephemeralPubkeyX: bytes32(uint256(0x1234)),
                ephemeralPubkeyYParity: 0x02,
                ciphertext: new bytes(64),
                nonce: bytes12(0),
                tag: bytes16(0)
            })
        });
        qd = QueuedDeposit({
            depositType: DepositType.Encrypted, depositData: abi.encode(ed), rejected: false
        });
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
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs, new EnabledToken[](0));

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
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs, new EnabledToken[](0));

        // Invalid encrypted deposits bounce to Tempo via the outbox; no zone mint is attempted.
        assertEq(zoneToken.balanceOf(alice), 0);
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
            token: address(zoneToken),
            sender: alice,
            to: bob,
            amount: 100e6,
            bouncebackRecipient: bob,
            memo: bytes32("d1")
        });
        QueuedDeposit memory qdRegular = QueuedDeposit({
            depositType: DepositType.Regular, depositData: abi.encode(d), rejected: false
        });

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
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs, new EnabledToken[](0));

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
        inbox.advanceTempo("", deposits, emptyDecs, new EnabledToken[](0));
    }

    function test_advanceTempo_extraDecryptionData() public {
        // Build a regular deposit only (no encrypted deposits)
        Deposit memory d = Deposit({
            token: address(zoneToken),
            sender: alice,
            to: bob,
            amount: 100e6,
            bouncebackRecipient: bob,
            memo: bytes32("d1")
        });
        QueuedDeposit memory qd = QueuedDeposit({
            depositType: DepositType.Regular, depositData: abi.encode(d), rejected: false
        });

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
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });

        vm.prank(sequencer);
        vm.expectRevert(IZoneInbox.ExtraDecryptionData.selector);
        inbox.advanceTempo("", deposits, decs, new EnabledToken[](0));
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

    function test_advanceTempo_encryptedDeposit_invalidProof_bounces() public {
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
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs, new EnabledToken[](0));
        assertEq(zoneToken.balanceOf(alice), 0);
    }

    /*//////////////////////////////////////////////////////////////
                PLAINTEXT LENGTH VALIDATION TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Helper: set up an encrypted deposit flow where AES-GCM returns a specific plaintext
    function _setupEncryptedDepositWithPlaintext(
        bytes memory mockPlaintext,
        bool aesValid
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
            _setupEncryptedDepositWithPlaintext(shortPlaintext, true);

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs, new EnabledToken[](0));

        // Deposit should bounce via the outbox; no zone mint is attempted.
        assertEq(zoneToken.balanceOf(alice), 0, "sender should not receive a zone mint");
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
            _setupEncryptedDepositWithPlaintext(longPlaintext, true);

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs, new EnabledToken[](0));

        // Deposit should bounce via the outbox; no zone mint is attempted.
        assertEq(zoneToken.balanceOf(alice), 0, "sender should not receive a zone mint");
        assertEq(zoneToken.balanceOf(recipient), 0, "recipient should get nothing");
    }

    /// @notice Verify that an empty plaintext (0 bytes) causes the deposit to bounce
    function test_advanceTempo_encryptedDeposit_plaintextEmpty_bounces() public {
        address recipient = address(0x500);
        bytes32 memo = bytes32("secret memo");

        bytes memory emptyPlaintext = new bytes(0);

        (QueuedDeposit[] memory deposits, DecryptionData[] memory decs) =
            _setupEncryptedDepositWithPlaintext(emptyPlaintext, true);

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs, new EnabledToken[](0));

        // Deposit should bounce via the outbox; no zone mint is attempted.
        assertEq(zoneToken.balanceOf(alice), 0, "sender should not receive a zone mint");
        assertEq(zoneToken.balanceOf(recipient), 0, "recipient should get nothing");
    }

    /// @notice Verify that exactly 64-byte plaintext with correct data succeeds
    function test_advanceTempo_encryptedDeposit_plaintextExact64_succeeds() public {
        address recipient = address(0x500);
        bytes32 memo = bytes32("secret memo");

        // Use the canonical encodePlaintext which produces exactly 64 bytes
        bytes memory correctPlaintext = EncryptedDepositLib.encodePlaintext(recipient, memo);

        (QueuedDeposit[] memory deposits, DecryptionData[] memory decs) =
            _setupEncryptedDepositWithPlaintext(correctPlaintext, true);

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs, new EnabledToken[](0));

        // Deposit should succeed — minted to the decrypted recipient
        assertEq(zoneToken.balanceOf(recipient), 1000e6, "recipient should receive funds");
        assertEq(zoneToken.balanceOf(alice), 0, "sender should get nothing (successful deposit)");
    }

    function _advanceTempoQueued(
        QueuedDeposit[] memory deposits,
        DecryptionData[] memory decryptions,
        EnabledToken[] memory enabledTokens
    )
        internal
    {
        inbox.advanceTempo("", deposits, decryptions, enabledTokens);
    }

    /// @notice Advancing accepts an enabled token even if the portal has not enabled it.
    function test_advanceTempo_enabledTokenNotPortalEnabled_accepts() public {
        address token = address(0x777);
        vm.etch(TIP20_FACTORY_ADDRESS, hex"00");
        vm.mockCall(
            TIP20_FACTORY_ADDRESS,
            abi.encodeWithSelector(ITIP20ZoneFactory.enableToken.selector),
            abi.encode()
        );

        EnabledToken[] memory enabledTokens = new EnabledToken[](1);
        enabledTokens[0] =
            EnabledToken({ token: token, name: "Token", symbol: "TOK", currency: "USD" });

        vm.prank(sequencer);
        _advanceTempoQueued(new QueuedDeposit[](0), new DecryptionData[](0), enabledTokens);
    }

    /// @notice Advancing accepts duplicate enabled token entries.
    function test_advanceTempo_duplicateEnabledToken_accepts() public {
        address token = address(0x777);
        vm.etch(TIP20_FACTORY_ADDRESS, hex"00");
        vm.mockCall(
            TIP20_FACTORY_ADDRESS,
            abi.encodeWithSelector(ITIP20ZoneFactory.enableToken.selector),
            abi.encode()
        );

        EnabledToken[] memory enabledTokens = new EnabledToken[](2);
        enabledTokens[0] =
            EnabledToken({ token: token, name: "Token", symbol: "TOK", currency: "USD" });
        enabledTokens[1] = enabledTokens[0];

        vm.prank(sequencer);
        _advanceTempoQueued(new QueuedDeposit[](0), new DecryptionData[](0), enabledTokens);
    }

    /// @notice Claiming with no refund returns zero and mints nothing.
    function test_claimRefund_zeroAmount() public {
        vm.prank(alice);
        uint128 amount = inbox.claimRefund(address(zoneToken));

        assertEq(amount, 0);
        assertEq(zoneToken.balanceOf(alice), 0);
    }

    /// @notice Claiming pays a parked mint refund and clears it.
    function test_claimRefund_success() public {
        zoneToken.setMinter(address(inbox), false);
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            token: address(zoneToken),
            sender: alice,
            to: bob,
            amount: 100e6,
            bouncebackRecipient: address(0),
            memo: bytes32(0)
        });
        tempoState.setMockStorageValue(
            mockPortal,
            PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
            keccak256(abi.encode(DepositType.Regular, deposits[0], bytes32(0)))
        );

        vm.prank(sequencer);
        _advanceTempo(deposits);
        assertEq(inbox.refunds(address(zoneToken), bob), 100e6);

        zoneToken.setMinter(address(inbox), true);
        vm.prank(bob);
        uint128 amount = inbox.claimRefund(address(zoneToken));

        assertEq(amount, 100e6);
        assertEq(inbox.refunds(address(zoneToken), bob), 0);
        assertEq(zoneToken.balanceOf(bob), 100e6);
    }

    /// @notice Credited supply plus parked refunds equals processed deposit value.
    function testFuzz_advanceTempo_zoneSupplyInvariant(
        uint8 rawRegular,
        uint8 rawEncrypted
    )
        public
    {
        uint256 regularCount = bound(rawRegular, 0, 4);
        uint256 encryptedCount = bound(rawEncrypted, 0, 4);
        uint256 totalCount = regularCount + encryptedCount;
        vm.assume(totalCount > 0);

        address encryptedRecipient = address(0x500);
        _setupEncryptionKeyMock(0, keccak256("seq-key"), 0x03);
        _setupPrecompileMocks(encryptedRecipient, bytes32("memo"));

        QueuedDeposit[] memory deposits = new QueuedDeposit[](totalCount);
        DecryptionData[] memory decs = new DecryptionData[](encryptedCount);
        uint128 netCredited;
        bytes32 currentHash;

        for (uint256 i = 0; i < regularCount; i++) {
            Deposit memory d = Deposit({
                token: address(zoneToken),
                sender: alice,
                to: bob,
                amount: uint128((i + 1) * 10e6),
                bouncebackRecipient: bob,
                memo: bytes32(i)
            });
            deposits[i] = QueuedDeposit({
                depositType: DepositType.Regular, depositData: abi.encode(d), rejected: false
            });
            currentHash = keccak256(abi.encode(DepositType.Regular, d, currentHash));
            netCredited += d.amount;
        }

        for (uint256 i = 0; i < encryptedCount; i++) {
            uint128 amount = uint128((i + 1) * 20e6);
            (QueuedDeposit memory qd, EncryptedDeposit memory ed) =
                _makeEncryptedDeposit(alice, amount, 0);
            deposits[regularCount + i] = qd;
            decs[i] = DecryptionData({
                sharedSecret: bytes32(uint256(i + 1)),
                sharedSecretYParity: 0x02,
                cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
            });
            currentHash = keccak256(abi.encode(DepositType.Encrypted, ed, currentHash));
            netCredited += amount;
        }

        tempoState.setMockStorageValue(
            mockPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, currentHash
        );

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits, decs, new EnabledToken[](0));

        uint256 parkedRefunds = inbox.refunds(address(zoneToken), bob)
            + inbox.refunds(address(zoneToken), encryptedRecipient);
        assertEq(zoneToken.totalSupply() + parkedRefunds, netCredited);
    }

}
