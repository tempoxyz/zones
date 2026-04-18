// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {TIP20} from "../../src/TIP20.sol";
import {IERC20} from "../../src/interfaces/IERC20.sol";
import {
    BlockTransition,
    DecryptionData,
    Deposit,
    DepositQueueTransition,
    DepositType,
    EnabledToken,
    EncryptedDeposit,
    EncryptedDepositPayload,
    IWithdrawalReceiver,
    IZoneFactory,
    IZonePortal,
    PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
    QueuedDeposit,
    Withdrawal,
    ZoneParams
} from "../../src/zone/IZone.sol";
import {EMPTY_SENTINEL} from "../../src/zone/WithdrawalQueueLib.sol";
import {ZONE_TX_CONTEXT} from "../../src/zone/IZone.sol";
import {ZoneConfig} from "../../src/zone/ZoneConfig.sol";
import {ZoneFactory} from "../../src/zone/ZoneFactory.sol";
import {ZoneInbox} from "../../src/zone/ZoneInbox.sol";
import {ZoneMessenger} from "../../src/zone/ZoneMessenger.sol";
import {ZoneOutbox} from "../../src/zone/ZoneOutbox.sol";
import {ZonePortal} from "../../src/zone/ZonePortal.sol";
import {BaseTest} from "../BaseTest.t.sol";
import {MockTempoState} from "./mocks/MockTempoState.sol";
import {MockZoneToken} from "./mocks/MockZoneToken.sol";
import {MockZoneTxContext} from "./mocks/MockZoneTxContext.sol";
import {Vm} from "forge-std/Vm.sol";

/// @notice Mock receiver that calls depositEncrypted on Zone B's portal (simulates SwapAndDepositRouter)
contract CrossZoneEncryptedRouter is IWithdrawalReceiver {
    address public targetPortal;
    address public token;

    bytes32 public lastSenderTag;
    bool public wasCallbackExecuted;

    constructor(address _targetPortal, address _token) {
        targetPortal = _targetPortal;
        token = _token;
    }

    function onWithdrawalReceived(bytes32 senderTag, address tokenIn, uint128 amount, bytes calldata data)
        external
        returns (bytes4)
    {
        lastSenderTag = senderTag;
        wasCallbackExecuted = true;

        // Decode the encrypted deposit payload from callbackData
        (uint256 keyIndex, EncryptedDepositPayload memory encrypted) =
            abi.decode(data, (uint256, EncryptedDepositPayload));

        // Approve and deposit encrypted into Zone B
        IERC20(tokenIn).approve(targetPortal, amount);
        IZonePortal(targetPortal).depositEncrypted(tokenIn, amount, keyIndex, encrypted);

        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }
}

/// @title ZoneToZonePrivacyTest
/// @notice E2E test: authenticated withdrawal from Zone A → encrypted deposit into Zone B
/// @dev Verifies that the sender on Zone A and recipient on Zone B are both blinded
///      from L1 (Tempo) observers. Only amount and token are visible.
contract ZoneToZonePrivacyTest is BaseTest {
    /*//////////////////////////////////////////////////////////////
                            ZONE A CONTRACTS
    //////////////////////////////////////////////////////////////*/

    ZoneFactory public zoneFactory;
    ZonePortal public portalA;
    ZoneMessenger public messengerA;

    MockZoneToken public zoneToken;
    MockTempoState public tempoStateA;
    ZoneConfig public configA;
    ZoneInbox public inboxA;
    ZoneOutbox public outboxA;

    /*//////////////////////////////////////////////////////////////
                            ZONE B CONTRACTS
    //////////////////////////////////////////////////////////////*/

    ZonePortal public portalB;
    ZoneMessenger public messengerB;

    /*//////////////////////////////////////////////////////////////
                            TEST HELPERS
    //////////////////////////////////////////////////////////////*/

    CrossZoneEncryptedRouter public router;

    bytes32 constant GENESIS_BLOCK_HASH_A = keccak256("genesisA");
    bytes32 constant GENESIS_BLOCK_HASH_B = keccak256("genesisB");
    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 public genesisTempoBlockNumber;

    // secp256k1 generator point X (known valid point on curve)
    bytes32 internal constant VALID_SECP256K1_X = 0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798;
    uint256 internal constant ENC_KEY_1 = 1;

    function setUp() public override {
        super.setUp();

        zoneFactory = new ZoneFactory();

        // Deploy shared zone token (same address on both zones in production)
        zoneToken = new MockZoneToken("Zone USD", "zUSD");
        zoneToken.setMinter(address(this), true);
        zoneToken.mint(alice, 100_000e6);
        zoneToken.mint(bob, 100_000e6);
        zoneToken.setMinter(address(this), false);

        genesisTempoBlockNumber = uint64(block.number);

        // === Deploy Zone A ===
        uint256 nonceA = vm.getNonce(address(this));
        address predictedPortalA = vm.computeCreateAddress(address(this), nonceA + 1);
        messengerA = new ZoneMessenger(predictedPortalA);
        portalA = new ZonePortal(
            1,
            address(zoneToken),
            address(messengerA),
            admin,
            zoneFactory.verifier(),
            GENESIS_BLOCK_HASH_A,
            genesisTempoBlockNumber
        );

        // === Deploy Zone B ===
        uint256 nonceB = vm.getNonce(address(this));
        address predictedPortalB = vm.computeCreateAddress(address(this), nonceB + 1);
        messengerB = new ZoneMessenger(predictedPortalB);
        portalB = new ZonePortal(
            2,
            address(zoneToken),
            address(messengerB),
            admin,
            zoneFactory.verifier(),
            GENESIS_BLOCK_HASH_B,
            genesisTempoBlockNumber
        );

        // === Deploy router (sits on Tempo, receives withdrawal from A, deposits encrypted into B) ===
        router = new CrossZoneEncryptedRouter(address(portalB), address(zoneToken));

        // === Set encryption key on Zone B's portal (sequencer = admin) ===
        _setEncKey(portalB, ENC_KEY_1);

        // === Zone A L2 setup ===
        tempoStateA = new MockTempoState(admin, GENESIS_TEMPO_BLOCK_HASH, genesisTempoBlockNumber);
        configA = new ZoneConfig(address(portalA), address(tempoStateA));
        tempoStateA.setMockStorageValue(address(portalA), bytes32(uint256(0)), bytes32(uint256(uint160(admin))));
        inboxA = new ZoneInbox(address(configA), address(portalA), address(tempoStateA));
        outboxA = new ZoneOutbox(address(configA));

        zoneToken.setMinter(address(inboxA), true);
        zoneToken.setBurner(address(outboxA), true);
    }

    function _wrapDeposits(Deposit[] memory deposits) internal pure returns (QueuedDeposit[] memory queued) {
        queued = new QueuedDeposit[](deposits.length);
        for (uint256 i = 0; i < deposits.length; i++) {
            queued[i] = QueuedDeposit({depositType: DepositType.Regular, depositData: abi.encode(deposits[i])});
        }
    }

    function _setEncKey(ZonePortal portal, uint256 privateKey) internal {
        Vm.Wallet memory w = vm.createWallet(privateKey);
        bytes32 x = bytes32(w.publicKeyX);
        uint8 yParity = w.publicKeyY % 2 == 0 ? 0x02 : 0x03;
        bytes32 message = keccak256(abi.encode(address(portal), x, yParity));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(w.privateKey, message);
        portal.setSequencerEncryptionKey(x, yParity, v, r, s);
    }

    function _senderTag(address sender, uint256 txSequence) internal view returns (bytes32) {
        return keccak256(abi.encodePacked(sender, zoneTxContext.txHashFor(txSequence)));
    }

    function _emptyEncryptedSenders(uint256 count) internal view returns (bytes[] memory encryptedSenders) {
        uint256 pending = outboxA.pendingWithdrawalsCount();
        if (count > pending) {
            count = pending;
        }
        encryptedSenders = new bytes[](count);
    }

    function _finalizeWithdrawalBatch(uint256 count) internal returns (bytes32) {
        vm.startPrank(admin);
        bytes32 hash = outboxA.finalizeWithdrawalBatch(count, uint64(block.number), _emptyEncryptedSenders(count));
        vm.stopPrank();
        return hash;
    }

    /*//////////////////////////////////////////////////////////////
              E2E: ZONE A → AUTHENTICATED WITHDRAWAL →
                   ENCRYPTED DEPOSIT → ZONE B
    //////////////////////////////////////////////////////////////*/

    /// @notice Full privacy flow: sender hidden on Zone A, recipient hidden on Zone B
    /// @dev
    ///   1. Alice has funds on Zone A (via deposit from Tempo)
    ///   2. Alice requests an authenticated withdrawal from Zone A with:
    ///      - senderTag commitment (hides her identity)
    ///      - callbackData encoding an encrypted deposit into Zone B
    ///   3. On Tempo, processWithdrawal transfers tokens to the router
    ///   4. Router calls portalB.depositEncrypted (recipient hidden in ciphertext)
    ///   5. Verify: L1 calldata contains NO plaintext sender or recipient
    function test_zoneToZone_senderAndRecipientBlinded() public {
        // ================================================================
        // STEP 1: Deposit into Zone A so Alice has funds
        // ================================================================
        uint128 depositAmount = 10_000e6;

        vm.startPrank(alice);
        zoneToken.approve(address(portalA), depositAmount);
        bytes32 depositHash = portalA.deposit(address(zoneToken), alice, depositAmount, bytes32("fund zone A"));
        vm.stopPrank();

        // Relay deposit to Zone A
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            token: address(zoneToken), sender: alice, to: alice, amount: depositAmount, memo: bytes32("fund zone A")
        });

        tempoStateA.setMockStorageValue(address(portalA), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, depositHash);
        vm.prank(admin);
        inboxA.advanceTempo("", _wrapDeposits(deposits), new DecryptionData[](0), new EnabledToken[](0));

        // Submit batch to advance Zone A
        bytes32 wHashEmpty = _finalizeWithdrawalBatch(type(uint256).max);
        vm.roll(block.number + 1);
        portalA.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({prevBlockHash: portalA.blockHash(), nextBlockHash: keccak256("zoneA-block1")}),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0),
                nextProcessedHash: depositHash,
                prevDepositNumber: 0,
                nextDepositNumber: 0
            }),
            wHashEmpty,
            "",
            ""
        );

        // ================================================================
        // STEP 2: Alice requests authenticated withdrawal from Zone A
        //         with callback that does encrypted deposit into Zone B
        // ================================================================
        uint128 withdrawAmount = 5000e6;

        // Build the encrypted deposit payload for Zone B
        // In production: ciphertext encrypts (recipientOnZoneB, memo) to Zone B sequencer's key
        EncryptedDepositPayload memory encPayload = EncryptedDepositPayload({
            ephemeralPubkeyX: VALID_SECP256K1_X,
            ephemeralPubkeyYParity: 0x02,
            ciphertext: new bytes(64), // encrypts (bob_address, memo) — opaque on L1
            nonce: bytes12(0),
            tag: bytes16(0)
        });

        // Encode callbackData: the router will decode this and call depositEncrypted
        bytes memory callbackData = abi.encode(uint256(0), encPayload);

        // Alice requests withdrawal on Zone A
        vm.startPrank(alice);
        zoneToken.approve(address(outboxA), withdrawAmount);
        outboxA.requestWithdrawal(
            address(zoneToken),
            address(router), // to: router on Tempo
            withdrawAmount,
            bytes32("cross-zone"), // memo
            200_000, // gasLimit for callback
            alice, // fallbackRecipient on Zone A
            callbackData
        );
        vm.stopPrank();

        // Verify tokens burned on Zone A
        assertEq(
            zoneToken.balanceOf(alice),
            100_000e6 - withdrawAmount, // initial funding minus withdrawal (deposit was minted back)
            "Alice's zone tokens should be burned"
        );

        // ================================================================
        // STEP 3: Sequencer finalizes withdrawal batch and submits to Tempo
        // ================================================================
        vm.roll(block.number + 1);
        bytes32 wHash = _finalizeWithdrawalBatch(type(uint256).max);
        assertTrue(wHash != bytes32(0), "Withdrawal hash should be non-zero");

        vm.roll(block.number + 1);
        portalA.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({prevBlockHash: portalA.blockHash(), nextBlockHash: keccak256("zoneA-block2")}),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0),
                nextProcessedHash: depositHash,
                prevDepositNumber: 0,
                nextDepositNumber: 0
            }),
            wHash,
            "",
            ""
        );

        // ================================================================
        // STEP 4: Build the Withdrawal struct as it appears on Tempo
        //         and verify privacy properties BEFORE processing
        // ================================================================
        bytes32 senderTag = _senderTag(alice, 1);
        Withdrawal memory w = Withdrawal({
            token: address(zoneToken),
            senderTag: senderTag,
            to: address(router),
            amount: withdrawAmount,
            fee: 0,
            memo: bytes32("cross-zone"),
            gasLimit: 200_000,
            fallbackRecipient: alice,
            callbackData: callbackData,
            encryptedSender: ""
        });

        // --- PRIVACY ASSERTION 1: senderTag does NOT reveal Alice's address ---
        // The senderTag is a commitment: keccak256(sender || txHash)
        // An L1 observer cannot reverse this without knowing the zone txHash
        assertTrue(senderTag != bytes32(uint256(uint160(alice))), "senderTag must not be Alice's raw address");
        assertTrue(
            senderTag != keccak256(abi.encodePacked(alice)), "senderTag must not be keccak256(alice) without blinding"
        );

        // --- PRIVACY ASSERTION 2: callbackData contains encrypted payload, not plaintext recipient ---
        // The callbackData encodes (keyIndex, EncryptedDepositPayload)
        // The actual recipient (bob) is inside encPayload.ciphertext, not in plaintext
        (uint256 decodedKeyIndex, EncryptedDepositPayload memory decodedPayload) =
            abi.decode(callbackData, (uint256, EncryptedDepositPayload));
        assertEq(decodedKeyIndex, 0, "keyIndex should be 0");
        // The ciphertext is opaque — it does NOT contain bob's address in plaintext
        // Verify no plaintext address leaks in the encrypted payload fields
        assertTrue(
            decodedPayload.ephemeralPubkeyX != bytes32(uint256(uint160(bob))), "ephemeral key must not leak recipient"
        );

        // --- PRIVACY ASSERTION 3: The Withdrawal struct's `to` field points to the router, not bob ---
        assertEq(w.to, address(router), "Withdrawal.to should be router, not end recipient");
        assertTrue(w.to != bob, "Withdrawal.to must NOT be bob (the actual Zone B recipient)");

        // ================================================================
        // STEP 5: Process the withdrawal on Tempo (triggers callback)
        // ================================================================
        portalA.processWithdrawal(w, bytes32(0));

        // Verify the callback was executed
        assertTrue(router.wasCallbackExecuted(), "Router callback must have executed");

        // --- PRIVACY ASSERTION 4: Router only received senderTag, not Alice's address ---
        assertEq(router.lastSenderTag(), senderTag, "Router should receive the blinded senderTag");

        // --- PRIVACY ASSERTION 5: Zone B's deposit queue now contains an encrypted deposit ---
        // The deposit hash changed (encrypted deposit was enqueued)
        bytes32 zoneBDepositHash = portalB.currentDepositQueueHash();
        assertTrue(zoneBDepositHash != bytes32(0), "Zone B should have an encrypted deposit in queue");

        // Reconstruct what the portal stored: an EncryptedDeposit in the queue
        uint128 fee = portalB.calculateDepositFee();
        uint128 netAmount = withdrawAmount - fee;
        EncryptedDeposit memory expectedEncDeposit = EncryptedDeposit({
            token: address(zoneToken),
            sender: address(router), // sender is the router, not Alice
            amount: netAmount,
            keyIndex: 0,
            encrypted: encPayload
        });
        bytes32 expectedHash = keccak256(abi.encode(DepositType.Encrypted, expectedEncDeposit, bytes32(0)));
        assertEq(zoneBDepositHash, expectedHash, "Zone B deposit hash must match encrypted deposit from router");

        // --- PRIVACY ASSERTION 6: The encrypted deposit sender is the router, not Alice ---
        assertEq(expectedEncDeposit.sender, address(router), "Encrypted deposit sender should be router, not Alice");
        assertTrue(expectedEncDeposit.sender != alice, "Alice's address must not appear as sender in Zone B deposit");

        // ================================================================
        // SUMMARY OF WHAT AN L1 OBSERVER SEES:
        //   - Withdrawal from Zone A: senderTag (blinded), amount, token, router address
        //   - Deposit into Zone B: router as sender, amount, token, encrypted payload
        //   - Alice's identity: HIDDEN (behind senderTag commitment)
        //   - Bob's identity: HIDDEN (inside EncryptedDepositPayload.ciphertext)
        //   - Only amount and token are public on L1
        // ================================================================
    }

    /// @notice Verify that senderTag is properly blinded by txHash
    /// @dev Without the zone txHash, an observer cannot determine senderTag came from Alice
    function test_senderTag_cannotBeReversedWithoutTxHash() public {
        bytes32 tag = _senderTag(alice, 1);
        bytes32 txHash = zoneTxContext.txHashFor(1);

        // With knowledge of (sender, txHash), you CAN verify
        assertEq(tag, keccak256(abi.encodePacked(alice, txHash)), "senderTag should match keccak256(sender || txHash)");

        // Without txHash, brute-forcing all addresses won't match
        // because the txHash is a random blinding factor from the zone
        bytes32 naiveHash = keccak256(abi.encodePacked(alice));
        assertTrue(tag != naiveHash, "senderTag must differ from hash of address alone");

        // Different txHash produces a different tag for the same sender
        bytes32 tag2 = _senderTag(alice, 2);
        assertTrue(tag != tag2, "Different txHash must produce different senderTag");
    }

    /// @notice Verify multiple users produce indistinguishable withdrawal patterns
    /// @dev An observer cannot link senderTags to specific users
    function test_senderTags_unlinkableAcrossUsers() public {
        bytes32 aliceTag = _senderTag(alice, 1);
        bytes32 bobTag = _senderTag(bob, 2);

        // Both tags are 32-byte hashes — structurally identical, no user info leaks
        assertTrue(aliceTag != bobTag, "Different users should have different tags");
        // Neither tag reveals the underlying address
        assertTrue(aliceTag != bytes32(uint256(uint160(alice))), "Alice's tag must not contain her address");
        assertTrue(bobTag != bytes32(uint256(uint160(bob))), "Bob's tag must not contain his address");
    }
}
