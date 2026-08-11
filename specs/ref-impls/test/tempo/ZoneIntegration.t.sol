// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    AES_GCM_DECRYPT,
    BlockTransition,
    CHAUM_PEDERSEN_VERIFY,
    ChaumPedersenProof,
    DecryptionData,
    Deposit,
    DepositPayload,
    DepositQueueTransition,
    DepositType,
    EnabledToken,
    EncryptionKeyEntry,
    IAesGcmDecrypt,
    IChaumPedersenVerify,
    IWithdrawalReceiver,
    IZoneFactory,
    IZonePortal,
    PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
    PORTAL_ENCRYPTION_KEYS_SLOT,
    PORTAL_IS_SEQUENCER_SLOT,
    QueuedDeposit,
    Withdrawal,
    ZONE_MESSENGER_ADDRESS,
    ZONE_VERIFIER_ADDRESS,
    ZoneInfo
} from "../../src/interfaces/IZone.sol";
import { EMPTY_SENTINEL } from "../../src/libraries/WithdrawalQueueLib.sol";
import { ZoneMessenger } from "../../src/tempo/ZoneMessenger.sol";
import { ZonePortal } from "../../src/tempo/ZonePortal.sol";
import { ZoneInbox } from "../../src/zone/ZoneInbox.sol";
import { ZoneOutbox } from "../../src/zone/ZoneOutbox.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { GatewayCallbackData, GatewayFlow } from "../mocks/MockZoneGateway.sol";
import { Vm } from "forge-std/Vm.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";

import { MockTempoState } from "../mocks/MockTempoState.sol";
import { MockZoneToken } from "../mocks/MockZoneToken.sol";

/// @notice Cleartext input used to construct deposit payloads in integration tests.
struct IntegrationDepositFixture {
    address token;
    address sender;
    address to;
    uint128 amount;
    address tempoRefundRecipient;
    bytes32 memo;
}

/// @notice Mock receiver that tracks received amounts
contract TrackingReceiver is IWithdrawalReceiver {

    uint256 public totalReceived;
    uint256 public callCount;

    function onWithdrawalReceived(
        uint32,
        address,
        bytes32,
        address,
        uint128 amount,
        bytes calldata
    )
        external
        returns (bytes4)
    {
        totalReceived += amount;
        callCount++;
        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }

}

contract MockZoneFactoryForIntegrationMessenger {

    mapping(uint32 => ZoneInfo) internal _zones;

    function setPortal(uint32 zoneId, address portal) external {
        _zones[zoneId].zoneId = zoneId;
        _zones[zoneId].portal = portal;
    }

    function zones(uint32 id) external view returns (ZoneInfo memory) {
        return _zones[id];
    }

}

/// @title ZoneIntegrationTest
/// @notice Comprehensive integration tests for the full zone lifecycle
contract ZoneIntegrationTest is BaseTest {

    // L1 contracts
    ZonePortal public l1Portal;

    // L2 contracts
    MockZoneToken public l2ZoneToken;
    MockTempoState public l2TempoState;
    ZoneInbox public l2Inbox;
    ZoneOutbox public l2Outbox;

    // Helpers
    TrackingReceiver public receiver;
    uint32 public zoneId;

    bytes32 constant GENESIS_BLOCK_HASH = keccak256("genesis");
    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 public genesisTempoBlockNumber;

    function _gatewayCallback() internal returns (bytes memory) {
        Vm.Wallet memory wallet = vm.createWallet(1);
        bytes32 x = bytes32(wallet.publicKeyX);
        uint8 yParity = wallet.publicKeyY % 2 == 0 ? 0x02 : 0x03;
        bytes32 message = keccak256(abi.encode(address(l1Portal), x, yParity));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(wallet.privateKey, message);
        l1Portal.setSequencerEncryptionKey(x, yParity, v, r, s);

        return abi.encode(
            GatewayCallbackData({
                flow: GatewayFlow.Deposit,
                outputToken: address(l2ZoneToken),
                keyIndex: 0,
                encrypted: DepositPayload({
                    ephemeralPubkeyX: x,
                    ephemeralPubkeyYParity: yParity,
                    ciphertext: new bytes(64),
                    nonce: bytes12(0),
                    tag: bytes16(0)
                }),
                minVaultAssets: 0,
                minVaultShares: 0,
                minOutputAmount: 0,
                actionId: bytes32(0),
                tempoRefundRecipient: alice
            })
        );
    }

    function setUp() public override {
        super.setUp();

        _installSharedZoneRuntimes();
        receiver = new TrackingReceiver();

        // Deploy zone token FIRST
        l2ZoneToken = new MockZoneToken("Zone USD", "zUSD");

        // Fund test accounts with zone token (for L1 deposits)
        l2ZoneToken.setMinter(address(this), true);
        l2ZoneToken.mint(alice, 1_000_000e6);
        l2ZoneToken.mint(bob, 1_000_000e6);
        l2ZoneToken.mint(charlie, 1_000_000e6);
        l2ZoneToken.setMinter(address(this), false);

        _mockTokenPolicyMigration(address(l2ZoneToken), true);

        genesisTempoBlockNumber = uint64(block.number);

        // Deploy portal directly (bypass factory TIP20 prefix check).
        ZoneMessenger messengerContract = ZoneMessenger(ZONE_MESSENGER_ADDRESS);
        l1Portal = new ZonePortal();
        address[] memory sequencers = new address[](1);
        sequencers[0] = sequencer;
        vm.prank(_ZONE_FACTORY);
        l1Portal.initialize(
            1,
            address(l2ZoneToken),
            true,
            true,
            _closedLoopAccounts(),
            _zoneGateways(),
            address(messengerContract),
            admin,
            sequencers,
            1,
            ZONE_VERIFIER_ADDRESS,
            ""
        );
        zoneId = 1;
        vm.mockCall(
            _ZONE_FACTORY,
            abi.encodeWithSelector(IZoneFactory.zones.selector, zoneId),
            abi.encode(
                ZoneInfo({
                    zoneId: zoneId,
                    portal: address(l1Portal),
                    accessMode: true,
                    gatewayMode: true,
                    admin: admin,
                    sequencers: sequencers,
                    threshold: 1,
                    verifier: ZONE_VERIFIER_ADDRESS,
                    rpcUrl: ""
                })
            )
        );

        // L2 setup
        l2TempoState =
            new MockTempoState(sequencer, GENESIS_TEMPO_BLOCK_HASH, genesisTempoBlockNumber);
        l2TempoState.setMockStorageValue(
            address(l1Portal),
            keccak256(abi.encode(sequencer, PORTAL_IS_SEQUENCER_SLOT)),
            bytes32(uint256(1))
        );
        l2TempoState.setMockTokenEnabled(address(l1Portal), address(l2ZoneToken), true);
        address[] memory accounts = _closedLoopAccounts();
        for (uint256 i; i < accounts.length; ++i) {
            l2TempoState.setMockAccountAllowed(address(l1Portal), accounts[i], true);
        }
        l2TempoState.setMockZoneGateway(address(l1Portal), address(zoneGateway), true);
        l2Inbox = new ZoneInbox(address(l1Portal), address(l2TempoState));
        l2Outbox = new ZoneOutbox(address(l1Portal), address(l2TempoState));

        l2ZoneToken.setMinter(address(l2Inbox), true);
        l2ZoneToken.setBurner(address(l2Outbox), true);
    }

    function _depositHash(
        IntegrationDepositFixture memory deposit,
        bytes32 previousHash
    )
        internal
        view
        returns (bytes32)
    {
        Deposit memory depositData = Deposit({
            token: deposit.token,
            sender: deposit.sender,
            amount: deposit.amount,
            tempoRefundRecipient: deposit.tempoRefundRecipient,
            keyIndex: l1Portal.encryptionKeyCount() - 1,
            encrypted: _depositPayload(deposit.to, deposit.memo)
        });
        return keccak256(abi.encode(DepositType.Deposit, depositData, previousHash));
    }

    function _wrapDeposits(IntegrationDepositFixture[] memory deposits)
        internal
        view
        returns (QueuedDeposit[] memory queued)
    {
        queued = new QueuedDeposit[](deposits.length);
        for (uint256 i = 0; i < deposits.length; i++) {
            Deposit memory depositData = Deposit({
                token: deposits[i].token,
                sender: deposits[i].sender,
                amount: deposits[i].amount,
                tempoRefundRecipient: deposits[i].tempoRefundRecipient,
                keyIndex: l1Portal.encryptionKeyCount() - 1,
                encrypted: _depositPayload(deposits[i].to, deposits[i].memo)
            });
            queued[i] = QueuedDeposit({
                depositType: DepositType.Deposit,
                depositData: abi.encode(depositData),
                rejected: false
            });
        }
    }

    function _advanceTempo(IntegrationDepositFixture[] memory deposits) internal {
        uint256 keyIndex = l1Portal.encryptionKeyCount() - 1;
        EncryptionKeyEntry memory key = l1Portal.encryptionKeyAt(keyIndex);
        uint256 base = uint256(keccak256(abi.encode(uint256(PORTAL_ENCRYPTION_KEYS_SLOT))));
        l2TempoState.setMockStorageValue(address(l1Portal), bytes32(base + keyIndex * 2), key.x);
        l2TempoState.setMockStorageValue(
            address(l1Portal), bytes32(base + keyIndex * 2 + 1), bytes32(uint256(key.yParity))
        );

        vm.etch(CHAUM_PEDERSEN_VERIFY, hex"00");
        vm.etch(AES_GCM_DECRYPT, hex"00");
        vm.mockCall(
            CHAUM_PEDERSEN_VERIFY,
            abi.encodeWithSelector(IChaumPedersenVerify.verifyProof.selector),
            abi.encode(true)
        );

        DecryptionData[] memory decryptions = new DecryptionData[](deposits.length);
        bytes[] memory decryptionResults = new bytes[](deposits.length);
        for (uint256 i; i < deposits.length; ++i) {
            decryptions[i] = DecryptionData({
                sharedSecret: bytes32(uint256(0xDEAD)),
                sharedSecretYParity: 0x02,
                cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
            });
            decryptionResults[i] =
                abi.encode(abi.encodePacked(deposits[i].to, deposits[i].memo, bytes12(0)), true);
        }
        vm.mockCalls(
            AES_GCM_DECRYPT,
            abi.encodeWithSelector(IAesGcmDecrypt.decrypt.selector),
            decryptionResults
        );

        QueuedDeposit[] memory queuedDeposits = _wrapDeposits(deposits);
        vm.prank(address(0));
        l2Inbox.advanceTempo("", queuedDeposits, decryptions, new EnabledToken[](0));
    }

    function _senderTag(address sender, uint256 txSequence) internal view returns (bytes32) {
        return keccak256(
            abi.encodePacked(sender, zoneTxContext.txHashFor(txSequence), uint64(txSequence))
        );
    }

    function _withdrawal(
        uint256 txSequence,
        address sender,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address zoneFallbackRecipient,
        bytes memory callbackData
    )
        internal
        view
        returns (Withdrawal memory)
    {
        return Withdrawal({
            token: address(l2ZoneToken),
            senderTag: _senderTag(sender, txSequence),
            to: to,
            amount: amount,
            memo: memo,
            gasLimit: gasLimit,
            fallbackNonce: uint64(txSequence),
            callbackData: callbackData,
            encryptedSender: ""
        });
    }

    function _emptyEncryptedSenders(uint256 count)
        internal
        view
        returns (bytes[] memory encryptedSenders)
    {
        encryptedSenders = new bytes[](count);
    }

    function _finalizeWithdrawalBatch(uint256 count) internal returns (bytes32) {
        if (count == type(uint256).max) {
            count = l2Outbox.pendingWithdrawalsCount();
        }
        vm.startPrank(sequencer);
        bytes32 hash = l2Outbox.finalizeWithdrawalBatch(
            count, uint64(block.number), _emptyEncryptedSenders(count)
        );
        vm.stopPrank();
        return hash;
    }

    /*//////////////////////////////////////////////////////////////
                   MULTI-USER DEPOSIT FLOW TESTS
    //////////////////////////////////////////////////////////////*/

    function test_multiUserDeposit_processedCorrectly() public {
        // Alice, Bob, Charlie all deposit
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 10_000e6);
        _deposit(l1Portal, address(l2ZoneToken), alice, 1000e6, bytes32("alice1"), alice);
        _deposit(l1Portal, address(l2ZoneToken), alice, 2000e6, bytes32("alice2"), alice);
        vm.stopPrank();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l1Portal), 5000e6);
        _deposit(l1Portal, address(l2ZoneToken), bob, 3000e6, bytes32("bob1"), bob);
        vm.stopPrank();

        vm.startPrank(charlie);
        l2ZoneToken.approve(address(l1Portal), 2000e6);
        _deposit(l1Portal, address(l2ZoneToken), charlie, 500e6, bytes32("charlie1"), charlie);
        vm.stopPrank();

        // Build deposit array
        IntegrationDepositFixture[] memory deposits = new IntegrationDepositFixture[](4);
        deposits[0] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 1000e6,
            tempoRefundRecipient: alice,
            memo: bytes32("alice1")
        });
        deposits[1] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 2000e6,
            tempoRefundRecipient: alice,
            memo: bytes32("alice2")
        });
        deposits[2] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: bob,
            to: bob,
            amount: 3000e6,
            tempoRefundRecipient: bob,
            memo: bytes32("bob1")
        });
        deposits[3] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: charlie,
            to: charlie,
            amount: 500e6,
            tempoRefundRecipient: charlie,
            memo: bytes32("charlie1")
        });

        // Set up L2 mock — hash chain uses l2ZoneToken consistently
        bytes32 l2h0 = bytes32(0);
        bytes32 l2h1 = _depositHash(deposits[0], l2h0);
        bytes32 l2h2 = _depositHash(deposits[1], l2h1);
        bytes32 l2h3 = _depositHash(deposits[2], l2h2);
        bytes32 l2h4 = _depositHash(deposits[3], l2h3);
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, l2h4
        );

        // Capture balances after L1 deposits but before L2 minting
        uint256 alicePre = l2ZoneToken.balanceOf(alice);
        uint256 bobPre = l2ZoneToken.balanceOf(bob);
        uint256 charliePre = l2ZoneToken.balanceOf(charlie);
        uint256 supplyPre = l2ZoneToken.totalSupply();

        // Process on L2
        _advanceTempo(deposits);

        // Verify L2 minting: each user receives their deposited amounts
        assertEq(l2ZoneToken.balanceOf(alice), alicePre + 3000e6);
        assertEq(l2ZoneToken.balanceOf(bob), bobPre + 3000e6);
        assertEq(l2ZoneToken.balanceOf(charlie), charliePre + 500e6);
        assertEq(l2ZoneToken.totalSupply(), supplyPre + 6500e6);
    }

    /*//////////////////////////////////////////////////////////////
               INCREMENTAL BATCH PROCESSING TESTS
    //////////////////////////////////////////////////////////////*/

    function test_incrementalBatchProcessing() public {
        // Batch 1: Two deposits
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 10_000e6);
        bytes32 d1 = _deposit(l1Portal, address(l2ZoneToken), alice, 1000e6, bytes32("d1"), alice);
        bytes32 d2 = _deposit(l1Portal, address(l2ZoneToken), alice, 2000e6, bytes32("d2"), alice);
        vm.stopPrank();

        // Process only first deposit
        IntegrationDepositFixture[] memory batch1 = new IntegrationDepositFixture[](1);
        batch1[0] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 1000e6,
            tempoRefundRecipient: alice,
            memo: bytes32("d1")
        });

        // IntegrationDepositFixture hash uses l2ZoneToken consistently
        bytes32 l2Hash1 = _depositHash(batch1[0], bytes32(0));
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, l2Hash1
        );
        uint256 alicePreBatch1 = l2ZoneToken.balanceOf(alice);
        _advanceTempo(batch1);

        assertEq(l2ZoneToken.balanceOf(alice), alicePreBatch1 + 1000e6);
        assertEq(l2Inbox.processedDepositQueueHash(), l2Hash1);

        // Submit L1 batch for first deposit
        vm.roll(block.number + 1);
        _submitBatch(
            l1Portal,
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("s1")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: d1,
                    prevDepositNumber: 0,
                    nextDepositNumber: 0
                }),
            bytes32(0),
            "",
            ""
        );

        // Portal no longer tracks processed hash - that's on the zone
        assertEq(l1Portal.currentDepositQueueHash(), d2);

        // More deposits arrive
        vm.prank(alice);
        _deposit(l1Portal, address(l2ZoneToken), alice, 3000e6, bytes32("d3"), alice);

        // Process remaining deposits
        IntegrationDepositFixture[] memory batch2 = new IntegrationDepositFixture[](2);
        batch2[0] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 2000e6,
            tempoRefundRecipient: alice,
            memo: bytes32("d2")
        });
        batch2[1] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 3000e6,
            tempoRefundRecipient: alice,
            memo: bytes32("d3")
        });

        // Compute L2 hash chain continuing from l2Hash1
        bytes32 l2Hash2 = _depositHash(batch2[0], l2Hash1);
        bytes32 l2Hash3 = _depositHash(batch2[1], l2Hash2);
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, l2Hash3
        );
        uint256 alicePreBatch2 = l2ZoneToken.balanceOf(alice);
        _advanceTempo(batch2);

        assertEq(l2ZoneToken.balanceOf(alice), alicePreBatch2 + 5000e6);
    }

    /*//////////////////////////////////////////////////////////////
              WITHDRAWAL WITH CALLBACK SUCCESS FLOW
    //////////////////////////////////////////////////////////////*/

    function test_withdrawalWithCallback_fullFlow() public {
        // Setup: Alice deposits
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 10_000e6);
        bytes32 depositHash =
            _deposit(l1Portal, address(l2ZoneToken), alice, 5000e6, bytes32("deposit"), alice);
        vm.stopPrank();

        // Process deposit on L2
        IntegrationDepositFixture[] memory deposits = new IntegrationDepositFixture[](1);
        deposits[0] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 5000e6,
            tempoRefundRecipient: alice,
            memo: bytes32("deposit")
        });
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, depositHash
        );
        _advanceTempo(deposits);

        bytes memory callbackData = _gatewayCallback();

        // Alice requests withdrawal to the configured gateway.
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 2000e6);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken),
            address(zoneGateway),
            2000e6,
            bytes32("payment"),
            5_000_000,
            alice,
            callbackData
        );
        vm.stopPrank();

        // Finalize L2 batch
        bytes32 withdrawalHash = _finalizeWithdrawalBatch(type(uint256).max);

        // Submit L1 batch
        vm.roll(block.number + 1);
        _submitBatch(
            l1Portal,
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("state")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: depositHash,
                    prevDepositNumber: 0,
                    nextDepositNumber: 0
                }),
            withdrawalHash,
            "",
            ""
        );

        // Process withdrawal
        Withdrawal memory w = _withdrawal(
            1,
            alice,
            address(zoneGateway),
            2000e6,
            bytes32("payment"),
            5_000_000,
            alice,
            callbackData
        );
        bytes32 depositHashBefore = l1Portal.currentDepositQueueHash();
        l1Portal.processWithdrawals(_singleWithdrawal(w), bytes32(0));

        assertNotEq(l1Portal.currentDepositQueueHash(), depositHashBefore);
        assertEq(l2ZoneToken.balanceOf(address(zoneGateway)), 0);
    }

    /*//////////////////////////////////////////////////////////////
                MULTIPLE BATCHES WITH WITHDRAWALS
    //////////////////////////////////////////////////////////////*/

    function test_multipleBatches_withdrawalsInDifferentSlots() public {
        // Initial deposit
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 100_000e6);
        bytes32 depositHash = _deposit(
            l1Portal, address(l2ZoneToken), alice, 50_000e6, bytes32("big deposit"), alice
        );
        vm.stopPrank();

        // Process on L2
        IntegrationDepositFixture[] memory deposits = new IntegrationDepositFixture[](1);
        deposits[0] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 50_000e6,
            tempoRefundRecipient: alice,
            memo: bytes32("big deposit")
        });
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, depositHash
        );
        _advanceTempo(deposits);

        // First batch: Alice withdraws to Bob
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 50_000e6);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken), bob, 1000e6, bytes32("to bob"), 0, alice, ""
        );
        vm.stopPrank();

        // Each finalizeWithdrawalBatch requires blockNumber == block.number, and each
        // batch needs a distinct block.number, so we advance before each finalize+submit pair.
        vm.roll(block.number + 1);

        bytes32 wHash1 = _finalizeWithdrawalBatch(type(uint256).max);

        _submitBatch(
            l1Portal,
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("s1")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: depositHash,
                    prevDepositNumber: 0,
                    nextDepositNumber: 0
                }),
            wHash1,
            "",
            ""
        );

        // Second batch: Alice withdraws to Charlie
        vm.startPrank(alice);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken), charlie, 2000e6, bytes32("to charlie"), 0, alice, ""
        );
        vm.stopPrank();

        vm.roll(block.number + 1);

        bytes32 wHash2 = _finalizeWithdrawalBatch(type(uint256).max);

        _submitBatch(
            l1Portal,
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("s2")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: depositHash,
                    prevDepositNumber: 0,
                    nextDepositNumber: 0
                }),
            wHash2,
            "",
            ""
        );

        // Third batch: Alice withdraws to herself
        vm.startPrank(alice);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken), alice, 3000e6, bytes32("to self"), 0, alice, ""
        );
        vm.stopPrank();

        vm.roll(block.number + 1);

        bytes32 wHash3 = _finalizeWithdrawalBatch(type(uint256).max);

        _submitBatch(
            l1Portal,
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("s3")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: depositHash,
                    prevDepositNumber: 0,
                    nextDepositNumber: 0
                }),
            wHash3,
            "",
            ""
        );

        // Verify queue state
        assertEq(l1Portal.withdrawalQueueHead(), 0);
        assertEq(l1Portal.withdrawalQueueTail(), 3);
        // Process in order (portal transfers l2ZoneToken from its balance)
        uint256 bobBefore = l2ZoneToken.balanceOf(bob);
        uint256 charlieBefore = l2ZoneToken.balanceOf(charlie);
        uint256 aliceBefore = l2ZoneToken.balanceOf(alice);

        Withdrawal memory w1 = _withdrawal(1, alice, bob, 1000e6, bytes32("to bob"), 0, alice, "");
        l1Portal.processWithdrawals(_singleWithdrawal(w1), bytes32(0));
        assertEq(l2ZoneToken.balanceOf(bob), bobBefore + 1000e6);

        Withdrawal memory w2 =
            _withdrawal(2, alice, charlie, 2000e6, bytes32("to charlie"), 0, alice, "");
        l1Portal.processWithdrawals(_singleWithdrawal(w2), bytes32(0));
        assertEq(l2ZoneToken.balanceOf(charlie), charlieBefore + 2000e6);

        Withdrawal memory w3 =
            _withdrawal(3, alice, alice, 3000e6, bytes32("to self"), 0, alice, "");
        l1Portal.processWithdrawals(_singleWithdrawal(w3), bytes32(0));
        assertEq(l2ZoneToken.balanceOf(alice), aliceBefore + 3000e6);

        // All processed
        assertEq(l1Portal.withdrawalQueueHead(), 3);
        assertFalse(l1Portal.withdrawalQueueHead() < l1Portal.withdrawalQueueTail());
    }

    /*//////////////////////////////////////////////////////////////
                    MIXED OPERATIONS FLOW
    //////////////////////////////////////////////////////////////*/

    function test_mixedFlow_depositsAndWithdrawalsInterleaved() public {
        // Phase 1: Initial deposits
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 100_000e6);
        _deposit(l1Portal, address(l2ZoneToken), alice, 10_000e6, bytes32("d1"), alice);
        vm.stopPrank();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l1Portal), 100_000e6);
        bytes32 d2 = _deposit(l1Portal, address(l2ZoneToken), bob, 5000e6, bytes32("d2"), bob);
        vm.stopPrank();

        // Process both deposits
        IntegrationDepositFixture[] memory deposits1 = new IntegrationDepositFixture[](2);
        deposits1[0] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 10_000e6,
            tempoRefundRecipient: alice,
            memo: bytes32("d1")
        });
        deposits1[1] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: bob,
            to: bob,
            amount: 5000e6,
            tempoRefundRecipient: bob,
            memo: bytes32("d2")
        });

        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, d2
        );
        _advanceTempo(deposits1);

        // Phase 2: Withdrawals
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 5000e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), charlie, 2000e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l2Outbox), 3000e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), charlie, 1500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        bytes32 wHash = _finalizeWithdrawalBatch(type(uint256).max);

        // Phase 3: More deposits arrive while withdrawals are pending
        vm.startPrank(charlie);
        l2ZoneToken.approve(address(l1Portal), 20_000e6);
        bytes32 d3 =
            _deposit(l1Portal, address(l2ZoneToken), charlie, 7500e6, bytes32("d3"), charlie);
        vm.stopPrank();

        // Submit batch with withdrawals
        vm.roll(block.number + 1);
        _submitBatch(
            l1Portal,
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("s1")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: d2,
                    prevDepositNumber: 0,
                    nextDepositNumber: 0
                }),
            wHash,
            "",
            ""
        );

        // Process new deposit
        IntegrationDepositFixture[] memory deposits2 = new IntegrationDepositFixture[](1);
        deposits2[0] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: charlie,
            to: charlie,
            amount: 7500e6,
            tempoRefundRecipient: charlie,
            memo: bytes32("d3")
        });

        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, d3
        );
        _advanceTempo(deposits2);

        // Verify all L2 balances (initial 1M - deposited + minted - burned)
        // alice: 1M - 10k + 10k - 2k = 998k; bob: 1M - 5k + 5k - 1.5k = 998.5k
        // charlie: 1M - 7.5k + 7.5k = 1M
        assertEq(l2ZoneToken.balanceOf(alice), 1_000_000e6 - 2000e6);
        assertEq(l2ZoneToken.balanceOf(bob), 1_000_000e6 - 1500e6);
        assertEq(l2ZoneToken.balanceOf(charlie), 1_000_000e6);

        // Process withdrawals
        Withdrawal memory w1 = _withdrawal(1, alice, charlie, 2000e6, bytes32(0), 0, alice, "");
        Withdrawal memory w2 = _withdrawal(2, bob, charlie, 1500e6, bytes32(0), 0, alice, "");

        bytes32 innerHash = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        uint256 charlieBefore = l2ZoneToken.balanceOf(charlie);

        l1Portal.processWithdrawals(_singleWithdrawal(w1), innerHash);
        l1Portal.processWithdrawals(_singleWithdrawal(w2), bytes32(0));

        assertEq(l2ZoneToken.balanceOf(charlie), charlieBefore + 3500e6);
    }

    /*//////////////////////////////////////////////////////////////
                       INVARIANT CHECKS
    //////////////////////////////////////////////////////////////*/

    function test_invariant_totalSupplyMatchesNetDeposits() public {
        // Initial supply: 3 users × 1M = 3M (from setUp)
        uint256 initialSupply = l2ZoneToken.totalSupply();

        // IntegrationDepositFixture 10000
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 10_000e6);
        bytes32 d1 = _deposit(l1Portal, address(l2ZoneToken), alice, 10_000e6, bytes32("d1"), alice);
        vm.stopPrank();

        IntegrationDepositFixture[] memory deposits = new IntegrationDepositFixture[](1);
        deposits[0] = IntegrationDepositFixture({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 10_000e6,
            tempoRefundRecipient: alice,
            memo: bytes32("d1")
        });
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, d1
        );
        _advanceTempo(deposits);

        assertEq(l2ZoneToken.totalSupply(), initialSupply + 10_000e6);

        // Withdraw 3000
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 3000e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), bob, 3000e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(l2ZoneToken.totalSupply(), initialSupply + 10_000e6 - 3000e6); // Tokens burned on withdrawal request

        // Transfer on L2 shouldn't change supply
        vm.prank(alice);
        l2ZoneToken.transfer(bob, 2000e6);

        assertEq(l2ZoneToken.totalSupply(), initialSupply + 10_000e6 - 3000e6);
    }

    /*//////////////////////////////////////////////////////////////
                    STORAGE LAYOUT VERIFICATION TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Verify PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT matches the actual ZonePortal storage layout.
    /// @dev If ZonePortal's storage layout changes, this test will fail.
    function test_storageLayout_currentDepositQueueHashSlot() public {
        // Make a deposit to get a non-zero currentDepositQueueHash
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        _deposit(l1Portal, address(l2ZoneToken), alice, 1000e6, bytes32("layout-test"), alice);
        vm.stopPrank();

        // Read via vm.load using our constant
        bytes32 fromSlot = vm.load(address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT);

        // Compare against the public getter
        assertEq(
            fromSlot,
            l1Portal.currentDepositQueueHash(),
            "PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT does not match actual storage position"
        );

        // Sanity: value should be non-zero after deposit
        assertTrue(fromSlot != bytes32(0), "deposit queue hash should be non-zero after deposit");
    }

}
