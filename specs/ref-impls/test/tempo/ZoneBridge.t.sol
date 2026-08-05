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
    IZoneInbox,
    IZonePortal,
    PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
    PORTAL_ENCRYPTION_KEYS_SLOT,
    PORTAL_IS_SEQUENCER_SLOT,
    QueuedDeposit,
    Withdrawal,
    ZONE_INBOX,
    ZONE_MESSENGER_ADDRESS,
    ZONE_OUTBOX,
    ZONE_VERIFIER_ADDRESS,
    ZoneInfo
} from "../../src/interfaces/IZone.sol";
import { EncryptedDepositLib } from "../../src/libraries/EncryptedDeposit.sol";
import { EMPTY_SENTINEL } from "../../src/libraries/WithdrawalQueueLib.sol";
import { ZoneMessenger } from "../../src/tempo/ZoneMessenger.sol";
import { ZonePortal } from "../../src/tempo/ZonePortal.sol";
import { ZoneInbox } from "../../src/zone/ZoneInbox.sol";
import { ZoneOutbox } from "../../src/zone/ZoneOutbox.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { MockTempoState } from "../mocks/MockTempoState.sol";
import { GatewayCallbackData, GatewayFlow } from "../mocks/MockZoneGateway.sol";
import { MockZoneToken } from "../mocks/MockZoneToken.sol";
import { Vm } from "forge-std/Vm.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";

/// @notice Cleartext input used to construct deposit payloads in bridge tests.
struct BridgeDepositFixture {
    address token;
    address sender;
    address to;
    uint128 amount;
    address tempoRefundRecipient;
    bytes32 memo;
}

/// @notice Mock withdrawal receiver for callback tests
contract MockWithdrawalReceiver is IWithdrawalReceiver {

    bool public shouldAccept = true;
    uint32 public lastZoneId;
    address public lastSourcePortal;
    bytes32 public lastSenderTag;
    address public lastToken;
    uint128 public lastAmount;
    bytes public lastCallbackData;

    function setShouldAccept(bool _accept) external {
        shouldAccept = _accept;
    }

    function onWithdrawalReceived(
        uint32 zoneId,
        address sourcePortal,
        bytes32 senderTag,
        address token,
        uint128 amount,
        bytes calldata callbackData
    )
        external
        returns (bytes4)
    {
        lastZoneId = zoneId;
        lastSourcePortal = sourcePortal;
        lastSenderTag = senderTag;
        lastToken = token;
        lastAmount = amount;
        lastCallbackData = callbackData;
        return shouldAccept ? IWithdrawalReceiver.onWithdrawalReceived.selector : bytes4(0);
    }

}

contract MockZoneFactoryForBridgeMessenger {

    mapping(uint32 => ZoneInfo) internal _zones;

    function setPortal(uint32 zoneId, address portal) external {
        _zones[zoneId].zoneId = zoneId;
        _zones[zoneId].portal = portal;
    }

    function zones(uint32 id) external view returns (ZoneInfo memory) {
        return _zones[id];
    }

}

/// @title ZoneBridgeTest
/// @notice Tests the full L1<->zone state machine with mocked message passing
/// @dev Simulates sequencer relaying data between chains asynchronously
contract ZoneBridgeTest is BaseTest {

    /*//////////////////////////////////////////////////////////////
                              L1 CONTRACTS
    //////////////////////////////////////////////////////////////*/

    ZonePortal public l1Portal;

    /*//////////////////////////////////////////////////////////////
                             ZONE CONTRACTS
    //////////////////////////////////////////////////////////////*/

    MockZoneToken public l2ZoneToken;
    MockTempoState public l2TempoState;
    ZoneInbox public l2Inbox;
    ZoneOutbox public l2Outbox;

    /*//////////////////////////////////////////////////////////////
                             TEST HELPERS
    //////////////////////////////////////////////////////////////*/

    MockWithdrawalReceiver public withdrawalReceiver;
    uint32 public zoneId;

    bytes32 constant GENESIS_BLOCK_HASH = keccak256("genesis");
    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 public genesisTempoBlockNumber;

    /// @notice Represents an observed deposit from Tempo (simulating sequencer watching events)
    struct ObservedDeposit {
        BridgeDepositFixture deposit;
        bytes32 newCurrentDepositQueueHash;
    }

    /// @notice Represents an observed withdrawal from zone events
    struct ObservedWithdrawal {
        uint64 index;
        Withdrawal withdrawal;
    }

    /// @notice Sequencer's pending deposit observations
    ObservedDeposit[] internal pendingDeposits;

    /// @notice Sequencer's observed withdrawals for current batch
    ObservedWithdrawal[] internal pendingWithdrawals;

    /// @notice Track zone block hash (in reality from block header)
    bytes32 internal l2BlockHash;

    function setUp() public override {
        super.setUp();

        // === Deploy L1 Contracts ===
        _installSharedZoneRuntimes();
        withdrawalReceiver = new MockWithdrawalReceiver();

        // Deploy zone token FIRST (used for both L1 escrow and zone-side operations).
        // In production, L1 and zone-side tokens are at the same address, so we use
        // a single MockZoneToken for both roles to avoid ISSUER_ROLE issues with pathUSD.
        l2ZoneToken = new MockZoneToken("Zone USD", "zUSD");

        // Fund test accounts with zone token (for L1 deposits)
        l2ZoneToken.setMinter(address(this), true);
        l2ZoneToken.mint(alice, 100_000e6);
        l2ZoneToken.mint(bob, 100_000e6);
        l2ZoneToken.setMinter(address(this), false);

        _mockTokenPolicyMigration(address(l2ZoneToken), true);

        // Record genesis block number for Tempo
        genesisTempoBlockNumber = uint64(block.number);

        // Deploy portal directly (bypass factory to avoid TIP20 prefix check).
        address[] memory bridgeAccounts = new address[](8);
        bridgeAccounts[0] = address(this);
        bridgeAccounts[1] = admin;
        bridgeAccounts[2] = alice;
        bridgeAccounts[3] = bob;
        bridgeAccounts[4] = charlie;
        bridgeAccounts[5] = address(0x600);
        bridgeAccounts[6] = address(0x700);
        bridgeAccounts[7] = address(0x800);

        ZoneMessenger messengerContract = ZoneMessenger(ZONE_MESSENGER_ADDRESS);
        l1Portal = new ZonePortal();
        address[] memory sequencers = new address[](1);
        sequencers[0] = sequencer;
        vm.prank(_ZONE_FACTORY);
        l1Portal.initialize(
            1, // zoneId
            address(l2ZoneToken), // initialToken = MockZoneToken (NOT pathUSD)
            true,
            true,
            bridgeAccounts,
            _zoneGateways(),
            address(messengerContract),
            admin, // admin
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

        // === Deploy zone contracts ===
        // TempoState mock for testing
        l2TempoState =
            new MockTempoState(sequencer, GENESIS_TEMPO_BLOCK_HASH, genesisTempoBlockNumber);

        l2TempoState.setMockStorageValue(
            address(l1Portal),
            keccak256(abi.encode(sequencer, PORTAL_IS_SEQUENCER_SLOT)),
            bytes32(uint256(1))
        );
        l2TempoState.setMockTokenEnabled(address(l1Portal), address(l2ZoneToken), true);
        for (uint256 i; i < bridgeAccounts.length; ++i) {
            l2TempoState.setMockAccountAllowed(address(l1Portal), bridgeAccounts[i], true);
        }
        l2TempoState.setMockZoneGateway(address(l1Portal), address(zoneGateway), true);

        // Zone inbox (advances Tempo state and processes deposits)
        ZoneInbox inboxImpl = new ZoneInbox(address(l1Portal), address(l2TempoState));
        vm.etch(ZONE_INBOX, address(inboxImpl).code);
        l2Inbox = ZoneInbox(ZONE_INBOX);
        l2ZoneToken.setMinter(address(l2Inbox), true);

        // Zone outbox (handles withdrawals)
        ZoneOutbox outboxImpl = new ZoneOutbox(address(l1Portal), address(l2TempoState));
        vm.etch(ZONE_OUTBOX, address(outboxImpl).code);
        l2Outbox = ZoneOutbox(ZONE_OUTBOX);
        l2ZoneToken.setBurner(address(l2Outbox), true);

        // Initialize zone block hash
        l2BlockHash = GENESIS_BLOCK_HASH;
    }

    function _senderTag(address sender, uint256 txSequence) internal view returns (bytes32) {
        return keccak256(abi.encodePacked(sender, zoneTxContext.txHashFor(txSequence)));
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
                       SEQUENCER SIMULATION HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Simulate sequencer observing an encrypted deposit event on Tempo.
    function _sequencerObserveDeposit(
        address sender,
        address to,
        uint128 amount,
        bytes32 memo
    )
        internal
        returns (bytes32 newHash)
    {
        // Keep decrypted fields in the observation so the relay can mock AES output.
        BridgeDepositFixture memory d = BridgeDepositFixture({
            token: address(l2ZoneToken),
            sender: sender,
            to: to,
            amount: amount,
            tempoRefundRecipient: to,
            memo: memo
        });

        Deposit memory encryptedDeposit = Deposit({
            token: d.token,
            sender: d.sender,
            amount: d.amount,
            tempoRefundRecipient: d.tempoRefundRecipient,
            keyIndex: l1Portal.encryptionKeyCount() - 1,
            encrypted: _depositPayload(d.to, d.memo)
        });

        // Calculate the encrypted queue hash (matches what the portal computes).
        bytes32 prevHash = pendingDeposits.length > 0
            ? pendingDeposits[pendingDeposits.length - 1].newCurrentDepositQueueHash
            : l2Inbox.processedDepositQueueHash();

        newHash = keccak256(abi.encode(DepositType.Deposit, encryptedDeposit, prevHash));

        pendingDeposits.push(ObservedDeposit({ deposit: d, newCurrentDepositQueueHash: newHash }));
    }

    /// @notice Simulate sequencer relaying deposits to the zone (sequencer-only call)
    function _sequencerRelayDepositsToL2() internal returns (bytes32 newProcessedHash) {
        if (pendingDeposits.length == 0) return l2Inbox.processedDepositQueueHash();

        QueuedDeposit[] memory deposits = new QueuedDeposit[](pendingDeposits.length);
        DecryptionData[] memory decryptions = new DecryptionData[](pendingDeposits.length);
        bytes[] memory decryptionResults = new bytes[](pendingDeposits.length);
        for (uint256 i = 0; i < pendingDeposits.length; i++) {
            BridgeDepositFixture memory d = pendingDeposits[i].deposit;
            Deposit memory ed = Deposit({
                token: d.token,
                sender: d.sender,
                amount: d.amount,
                tempoRefundRecipient: d.tempoRefundRecipient,
                keyIndex: l1Portal.encryptionKeyCount() - 1,
                encrypted: _depositPayload(d.to, d.memo)
            });
            deposits[i] = QueuedDeposit({
                depositType: DepositType.Deposit, depositData: abi.encode(ed), rejected: false
            });
            decryptions[i] = DecryptionData({
                sharedSecret: bytes32(uint256(0xDEAD)),
                sharedSecretYParity: 0x02,
                cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
            });
            decryptionResults[i] =
                abi.encode(EncryptedDepositLib.encodePlaintext(d.to, d.memo), true);
        }

        // Get expected final hash
        newProcessedHash = pendingDeposits[pendingDeposits.length - 1].newCurrentDepositQueueHash;

        // Set up mock: TempoState will return this hash when reading from portal
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, newProcessedHash
        );

        uint256 keyIndex = l1Portal.encryptionKeyCount() - 1;
        EncryptionKeyEntry memory key = l1Portal.encryptionKeyAt(keyIndex);
        _setupEncryptionKeyMockOnZone(keyIndex, key.x, key.yParity);
        vm.etch(CHAUM_PEDERSEN_VERIFY, hex"00");
        vm.etch(AES_GCM_DECRYPT, hex"00");
        vm.mockCall(
            CHAUM_PEDERSEN_VERIFY,
            abi.encodeWithSelector(IChaumPedersenVerify.verifyProof.selector),
            abi.encode(true)
        );
        vm.mockCalls(
            AES_GCM_DECRYPT,
            abi.encodeWithSelector(IAesGcmDecrypt.decrypt.selector),
            decryptionResults
        );

        // Process on zone via the advanceTempo system call.
        vm.prank(address(0));
        l2Inbox.advanceTempo("", deposits, decryptions, new EnabledToken[](0));

        // Clear pending
        delete pendingDeposits;

        // Update zone block hash (simulated)
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "deposits", newProcessedHash));
    }

    /// @notice Simulate sequencer observing a withdrawal event on the zone
    function _sequencerObserveWithdrawal(
        uint64 index,
        address sender,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address zoneFallbackRecipient,
        bytes memory data
    )
        internal
    {
        pendingWithdrawals.push(
            ObservedWithdrawal({
                index: index,
                withdrawal: _withdrawal(
                    uint256(index) + 1,
                    sender,
                    to,
                    amount,
                    memo,
                    gasLimit,
                    zoneFallbackRecipient,
                    data
                )
            })
        );
    }

    /// @notice Build withdrawal queue hash from observed events (oldest = outermost)
    /// @dev Only used for verification in tests, actual hash is built by l2Outbox.finalizeWithdrawalBatch()
    function _buildWithdrawalQueueHash() internal view returns (bytes32 queueHash) {
        if (pendingWithdrawals.length == 0) return bytes32(0);

        // Build from newest to oldest (so oldest ends up outermost)
        // Innermost element wraps EMPTY_SENTINEL
        queueHash = EMPTY_SENTINEL;
        for (uint256 i = pendingWithdrawals.length; i > 0;) {
            unchecked {
                i--;
            }
            queueHash = keccak256(abi.encode(pendingWithdrawals[i].withdrawal, queueHash));
        }
    }

    /// @notice Simulate sequencer building and submitting a batch to Tempo
    function _sequencerSubmitBatch(bytes32 newProcessedDepositQueueHash) internal {
        // Sequencer calls finalizeWithdrawalBatch() on zone outbox to get withdrawal hash on-chain
        bytes32 withdrawalQueueHash = _finalizeWithdrawalBatch(type(uint256).max);

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        // Submit to Tempo
        _submitBatch(
            l1Portal,
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: l1Portal.blockHash(), nextBlockHash: l2BlockHash }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0),
                nextProcessedHash: newProcessedDepositQueueHash,
                prevDepositNumber: 0,
                nextDepositNumber: 0
            }),
            withdrawalQueueHash,
            "",
            ""
        );

        // Clear pending withdrawals observation (they're now in Tempo queue)
        delete pendingWithdrawals;
    }

    /// @notice Get withdrawal from pending list by index
    function _getWithdrawalByIndex(uint64 targetIndex) internal view returns (Withdrawal memory) {
        for (uint256 i = 0; i < pendingWithdrawals.length; i++) {
            if (pendingWithdrawals[i].index == targetIndex) {
                return pendingWithdrawals[i].withdrawal;
            }
        }
        revert("withdrawal not found");
    }

    /*//////////////////////////////////////////////////////////////
                    FULL STATE MACHINE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_fullFlow_depositAndWithdraw() public {
        // === STEP 1: Alice deposits on L1 ===
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        bytes32 l1DepositHash = _deposit(
            l1Portal, address(l2ZoneToken), alice, depositAmount, bytes32("hello zone"), alice
        );
        vm.stopPrank();

        // Verify L1 state
        assertEq(l1Portal.currentDepositQueueHash(), l1DepositHash);
        assertEq(l2ZoneToken.balanceOf(address(l1Portal)), depositAmount);

        // === STEP 2: Sequencer observes deposit (simulated event watching) ===
        _sequencerObserveDeposit(alice, alice, depositAmount, bytes32("hello zone"));

        // === STEP 3: Sequencer relays deposit to zone (sequencer-only call) ===
        bytes32 newProcessedHash = _sequencerRelayDepositsToL2();

        // Verify zone state (alice's net balance is unchanged: -deposit on L1, +mint on zone)
        assertEq(l2ZoneToken.balanceOf(alice), 100_000e6);
        assertEq(l2Inbox.processedDepositQueueHash(), newProcessedHash);
        assertEq(l2ZoneToken.totalSupply(), 200_000e6 + depositAmount);

        // === STEP 4: Submit batch to L1 (no withdrawals yet) ===
        _sequencerSubmitBatch(newProcessedHash);

        // Verify L1 batch state updated
        assertEq(l1Portal.withdrawalBatchIndex(), 1);
        assertEq(l1Portal.blockHash(), l2BlockHash);

        // === STEP 5: Alice requests withdrawal on zone ===
        uint128 withdrawAmount = 400e6;
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), withdrawAmount);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken),
            alice, // to (back to self on L1)
            withdrawAmount,
            bytes32(0), // memo
            0, // no callback
            alice, // fallback to self
            ""
        );
        vm.stopPrank();

        // Verify zone state - tokens burned (from alice's net balance of 100_000e6)
        assertEq(l2ZoneToken.balanceOf(alice), 100_000e6 - withdrawAmount);

        // === STEP 6: Sequencer observes withdrawal event ===
        _sequencerObserveWithdrawal(0, alice, alice, withdrawAmount, bytes32(0), 0, alice, "");

        // Update zone state root
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "withdrawal", 0));

        // === STEP 7: Submit batch with withdrawal ===
        _sequencerSubmitBatch(newProcessedHash);

        // Verify L1 queue updated
        assertEq(l1Portal.withdrawalBatchIndex(), 2);
        Withdrawal memory w = _withdrawal(1, alice, alice, withdrawAmount, bytes32(0), 0, alice, "");
        bytes32 expectedQueueHash = keccak256(abi.encode(w, EMPTY_SENTINEL));
        // Withdrawal should be in slot 0 (first batch with withdrawals)
        assertEq(l1Portal.withdrawalQueueSlot(0), expectedQueueHash);
        assertEq(l1Portal.withdrawalQueueTail(), 1);

        // === STEP 8: Sequencer processes withdrawal on L1 ===
        uint256 aliceL1BalanceBefore = l2ZoneToken.balanceOf(alice);
        l1Portal.processWithdrawals(_singleWithdrawal(w), bytes32(0)); // 0 = last item in slot

        // Verify Alice received funds on L1
        assertEq(l2ZoneToken.balanceOf(alice), aliceL1BalanceBefore + withdrawAmount);

        // Verify slot cleared and head advanced
        assertEq(l1Portal.withdrawalQueueSlot(0), EMPTY_SENTINEL);
        assertEq(l1Portal.withdrawalQueueHead(), 1);
    }

    function test_fullFlow_multipleDepositsAndWithdrawals() public {
        // === Alice and Bob both deposit ===
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 5000e6);
        _deposit(l1Portal, address(l2ZoneToken), alice, 2000e6, bytes32("alice1"), alice);
        vm.stopPrank();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l1Portal), 5000e6);
        _deposit(l1Portal, address(l2ZoneToken), bob, 3000e6, bytes32("bob1"), bob);
        vm.stopPrank();

        // Sequencer observes and relays
        _sequencerObserveDeposit(alice, alice, 2000e6, bytes32("alice1"));
        _sequencerObserveDeposit(bob, bob, 3000e6, bytes32("bob1"));
        bytes32 processedHash = _sequencerRelayDepositsToL2();

        // Verify zone balances (net: -deposit on L1, +mint on zone = initial funding)
        assertEq(l2ZoneToken.balanceOf(alice), 100_000e6);
        assertEq(l2ZoneToken.balanceOf(bob), 100_000e6);

        // Submit batch
        _sequencerSubmitBatch(processedHash);

        // === Both request withdrawals ===
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 500e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l2Outbox), 1000e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), bob, 1000e6, bytes32(0), 0, bob, "");
        vm.stopPrank();

        // Sequencer observes withdrawals
        _sequencerObserveWithdrawal(0, alice, alice, 500e6, bytes32(0), 0, alice, "");
        _sequencerObserveWithdrawal(1, bob, bob, 1000e6, bytes32(0), 0, bob, "");

        // Submit batch with both withdrawals
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "withdrawals"));
        _sequencerSubmitBatch(processedHash);

        // Build expected queue hash (oldest = outermost, innermost wraps EMPTY_SENTINEL)
        Withdrawal memory w0 = _withdrawal(1, alice, alice, 500e6, bytes32(0), 0, alice, "");
        Withdrawal memory w1 = _withdrawal(2, bob, bob, 1000e6, bytes32(0), 0, bob, "");
        bytes32 innerHash = keccak256(abi.encode(w1, EMPTY_SENTINEL));
        bytes32 queueHash = keccak256(abi.encode(w0, innerHash));
        // Both withdrawals are in slot 0 (same batch)
        assertEq(l1Portal.withdrawalQueueSlot(0), queueHash);

        // Process withdrawals in order
        uint256 aliceBefore = l2ZoneToken.balanceOf(alice);
        uint256 bobBefore = l2ZoneToken.balanceOf(bob);

        l1Portal.processWithdrawals(_singleWithdrawal(w0), innerHash);
        assertEq(l2ZoneToken.balanceOf(alice), aliceBefore + 500e6);

        l1Portal.processWithdrawals(_singleWithdrawal(w1), bytes32(0)); // 0 = last item in slot
        assertEq(l2ZoneToken.balanceOf(bob), bobBefore + 1000e6);
    }

    function test_fullFlow_withdrawalWithCallback() public {
        // Setup: deposit to zone
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        _deposit(l1Portal, address(l2ZoneToken), alice, 1000e6, bytes32(""), alice);
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        bytes32 processedHash = _sequencerRelayDepositsToL2();
        _sequencerSubmitBatch(processedHash);
        _setEncKeyOnL1(ENC_KEY_1);
        bytes memory callbackData = _callbackData(GatewayFlow.Deposit);

        // Request withdrawal with callback
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 500e6);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken),
            address(zoneGateway),
            500e6,
            bytes32(0), // memo
            5_000_000, // gasLimit for callback
            alice, // zoneFallbackRecipient on zone
            callbackData
        );
        vm.stopPrank();

        // Sequencer observes and submits
        _sequencerObserveWithdrawal(
            0, alice, address(zoneGateway), 500e6, bytes32(0), 5_000_000, alice, callbackData
        );
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "callback_withdrawal"));
        _sequencerSubmitBatch(processedHash);

        // Process withdrawal
        Withdrawal memory w = _withdrawal(
            1, alice, address(zoneGateway), 500e6, bytes32(0), 5_000_000, alice, callbackData
        );
        bytes32 depositHashBefore = l1Portal.currentDepositQueueHash();
        l1Portal.processWithdrawals(_singleWithdrawal(w), bytes32(0));

        assertNotEq(l1Portal.currentDepositQueueHash(), depositHashBefore);
        assertEq(l2ZoneToken.balanceOf(address(zoneGateway)), 0);
    }

    function test_fullFlow_callbackFailureBouncesAndAdvancesQueue() public {
        // Setup: deposit to zone
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        _deposit(l1Portal, address(l2ZoneToken), alice, 1000e6, bytes32(""), alice);
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        bytes32 processedHash = _sequencerRelayDepositsToL2();
        _sequencerSubmitBatch(processedHash);
        _setEncKeyOnL1(ENC_KEY_1);
        bytes memory callbackData = _callbackData(GatewayFlow.Redeem);

        // Request a callback withdrawal, then make the mock omit its return deposit.
        zoneGateway.setReturnToZone(false);
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 500e6);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken),
            address(zoneGateway),
            500e6,
            bytes32(0), // memo
            5_000_000,
            alice, // fallback recipient
            callbackData
        );
        vm.stopPrank();

        // Sequencer observes and submits
        _sequencerObserveWithdrawal(
            0, alice, address(zoneGateway), 500e6, bytes32(0), 5_000_000, alice, callbackData
        );
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "failing_callback"));
        _sequencerSubmitBatch(processedHash);

        bytes32 depositHashBefore = l1Portal.currentDepositQueueHash();

        Withdrawal memory w = _withdrawal(
            1, alice, address(zoneGateway), 500e6, bytes32(0), 5_000_000, alice, callbackData
        );
        l1Portal.processWithdrawals(_singleWithdrawal(w), bytes32(0));

        assertNotEq(l1Portal.currentDepositQueueHash(), depositHashBefore);
        assertEq(l1Portal.withdrawalQueueHead(), l1Portal.withdrawalQueueTail());
        assertEq(l2ZoneToken.balanceOf(address(zoneGateway)), 0);
    }

    function test_fullFlow_transferOnL2() public {
        // BridgeDepositFixture to Alice
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        _deposit(l1Portal, address(l2ZoneToken), alice, 1000e6, bytes32(""), alice);
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Alice transfers to Bob on zone
        vm.prank(alice);
        l2ZoneToken.transfer(bob, 300e6);

        // Verify zone balances (alice net = 100K, then -300e6 transfer; bob = 100K + 300e6)
        assertEq(l2ZoneToken.balanceOf(alice), 100_000e6 - 300e6);
        assertEq(l2ZoneToken.balanceOf(bob), 100_000e6 + 300e6);

        // Bob withdraws on zone
        vm.startPrank(bob);
        l2ZoneToken.approve(address(l2Outbox), 300e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), bob, 300e6, bytes32(0), 0, bob, "");
        vm.stopPrank();

        // Verify Bob's zone balance debited (100K + 300e6 received - 300e6 withdrawn)
        assertEq(l2ZoneToken.balanceOf(bob), 100_000e6);
        assertEq(l2Outbox.nextWithdrawalIndex(), 1);
    }

    function test_l2_insufficientBalanceReverts() public {
        // BridgeDepositFixture to Alice
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        _deposit(l1Portal, address(l2ZoneToken), alice, 1000e6, bytes32(""), alice);
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Alice tries to withdraw more than balance (net balance is 100_000e6 after deposit+mint)
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), type(uint256).max);
        vm.expectRevert(MockZoneToken.InsufficientBalance.selector);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken), alice, uint128(100_001e6), bytes32(0), 0, alice, ""
        );
        vm.stopPrank();
    }

    function test_l2_transferInsufficientBalance() public {
        // BridgeDepositFixture to Alice
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        _deposit(l1Portal, address(l2ZoneToken), alice, 1000e6, bytes32(""), alice);
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Alice tries to transfer more than balance (net balance is 100_000e6 after deposit+mint)
        vm.prank(alice);
        vm.expectRevert(MockZoneToken.InsufficientBalance.selector);
        l2ZoneToken.transfer(bob, 100_001e6);
    }

    function test_l2_callbackRequiresFallbackRecipient() public {
        // BridgeDepositFixture to Alice
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        _deposit(l1Portal, address(l2ZoneToken), alice, 1000e6, bytes32(""), alice);
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Try callback without fallback recipient
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 500e6);
        vm.expectRevert(ZoneOutbox.InvalidFallbackRecipient.selector);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken),
            address(withdrawalReceiver),
            500e6,
            bytes32(0), // memo
            5_000_000, // gasLimit > 0
            address(0), // invalid fallback
            ""
        );
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                    STORAGE LAYOUT VERIFICATION TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Verify PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT matches the actual ZonePortal storage layout.
    /// @dev This is a critical regression test. If ZonePortal's storage layout changes,
    ///      this test will fail, preventing silent slot mismatches.
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

    /*//////////////////////////////////////////////////////////////
            ENCRYPTED DEPOSIT INTEGRATION TESTS — HELPERS
    //////////////////////////////////////////////////////////////*/

    // secp256k1 generator point X (known valid point on curve)
    bytes32 internal constant VALID_SECP256K1_X =
        0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798;

    // Test private keys for encryption key PoP
    uint256 internal constant ENC_KEY_1 = 1;
    uint256 internal constant ENC_KEY_2 = 2;

    /// @notice Observed encrypted deposit from L1 (simulating sequencer watching events)
    struct ObservedUserDeposit {
        Deposit deposit;
        bytes32 newCurrentDepositQueueHash;
    }

    /// @notice Pending encrypted deposit observations
    ObservedUserDeposit[] internal pendingUserDeposits;

    /// @notice Helper: set encryption key on L1 portal with proof of possession
    function _setEncKeyOnL1(uint256 privateKey) internal returns (bytes32 x, uint8 yParity) {
        Vm.Wallet memory w = vm.createWallet(privateKey);
        x = bytes32(w.publicKeyX);
        yParity = w.publicKeyY % 2 == 0 ? 0x02 : 0x03;
        bytes32 message = keccak256(abi.encode(address(l1Portal), x, yParity));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(w.privateKey, message);
        l1Portal.setSequencerEncryptionKey(x, yParity, v, r, s);
    }

    /// @notice Helper: create an encrypted deposit payload
    function _makeDepositPayload() internal pure returns (DepositPayload memory) {
        return DepositPayload({
            ephemeralPubkeyX: VALID_SECP256K1_X,
            ephemeralPubkeyYParity: 0x02,
            ciphertext: new bytes(64),
            nonce: bytes12(0),
            tag: bytes16(0)
        });
    }

    function _callbackData(GatewayFlow flow) internal view returns (bytes memory) {
        return abi.encode(
            GatewayCallbackData({
                flow: flow,
                outputToken: address(l2ZoneToken),
                keyIndex: 0,
                encrypted: _makeDepositPayload(),
                minVaultAssets: 0,
                minVaultShares: 0,
                minOutputAmount: 0,
                actionId: bytes32(0),
                tempoRefundRecipient: alice
            })
        );
    }

    /// @notice Simulate sequencer observing an encrypted deposit event on L1
    function _sequencerObserveDeposit(
        address sender,
        uint128 netAmount,
        uint256 keyIndex,
        DepositPayload memory encrypted
    )
        internal
        returns (bytes32 newHash)
    {
        Deposit memory ed = Deposit({
            token: address(l2ZoneToken),
            sender: sender,
            amount: netAmount,
            tempoRefundRecipient: sender,
            keyIndex: keyIndex,
            encrypted: encrypted
        });

        // Calculate the new hash (matches what portal computes via DepositQueueLib)
        bytes32 prevHash;
        if (pendingDeposits.length > 0) {
            prevHash = pendingDeposits[pendingDeposits.length - 1].newCurrentDepositQueueHash;
        } else if (pendingUserDeposits.length > 0) {
            prevHash =
            pendingUserDeposits[pendingUserDeposits.length - 1].newCurrentDepositQueueHash;
        } else {
            prevHash = l2Inbox.processedDepositQueueHash();
        }

        newHash = keccak256(abi.encode(DepositType.Deposit, ed, prevHash));
        pendingUserDeposits.push(
            ObservedUserDeposit({ deposit: ed, newCurrentDepositQueueHash: newHash })
        );
    }

    /// @notice Set up encryption key mock storage on zone side so ZoneInbox._readEncryptionKey works
    function _setupEncryptionKeyMockOnZone(
        uint256 keyIndex,
        bytes32 keyX,
        uint8 keyYParity
    )
        internal
    {
        uint256 base = uint256(keccak256(abi.encode(uint256(PORTAL_ENCRYPTION_KEYS_SLOT))));
        uint256 slotX = base + (keyIndex * 2);
        uint256 slotMeta = slotX + 1;
        l2TempoState.setMockStorageValue(address(l1Portal), bytes32(slotX), keyX);
        l2TempoState.setMockStorageValue(
            address(l1Portal), bytes32(slotMeta), bytes32(uint256(keyYParity))
        );
    }

    /// @notice Set up precompile mocks for successful encrypted deposit decryption
    function _setupPrecompileMocksSuccess(address recipient, bytes32 memo) internal {
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

    /// @notice Set up precompile mocks for failed AES-GCM decryption (bounce)
    function _setupPrecompileMocksFail() internal {
        vm.etch(CHAUM_PEDERSEN_VERIFY, hex"00");
        vm.etch(AES_GCM_DECRYPT, hex"00");

        // Mock Chaum-Pedersen to return valid (proof is fine, decryption fails)
        vm.mockCall(
            CHAUM_PEDERSEN_VERIFY,
            abi.encodeWithSelector(IChaumPedersenVerify.verifyProof.selector),
            abi.encode(true)
        );

        // Mock AES-GCM to return failure
        vm.mockCall(
            AES_GCM_DECRYPT,
            abi.encodeWithSelector(IAesGcmDecrypt.decrypt.selector),
            abi.encode(new bytes(0), false)
        );
    }

    /// @notice Simulate sequencer relaying a single encrypted deposit to the zone
    /// @dev Handles all the mock setup, builds the unified queue entries, and calls advanceTempo
    function _sequencerRelayDepositsToL2(
        address decryptedTo,
        bytes32 decryptedMemo,
        bool shouldSucceed
    )
        internal
        returns (bytes32 newProcessedHash)
    {
        require(pendingUserDeposits.length > 0, "no encrypted deposits to relay");
        require(
            pendingDeposits.length == 0, "use _sequencerRelayMixedDepositsToL2 for mixed queues"
        );

        // Build queued deposits array
        QueuedDeposit[] memory queued = new QueuedDeposit[](pendingUserDeposits.length);
        DecryptionData[] memory decs = new DecryptionData[](pendingUserDeposits.length);

        for (uint256 i = 0; i < pendingUserDeposits.length; i++) {
            queued[i] = QueuedDeposit({
                depositType: DepositType.Deposit,
                depositData: abi.encode(pendingUserDeposits[i].deposit),
                rejected: false
            });
            decs[i] = DecryptionData({
                sharedSecret: bytes32(uint256(0xDEAD)),
                sharedSecretYParity: 0x02,
                cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
            });
        }

        // Get expected final hash
        newProcessedHash =
        pendingUserDeposits[pendingUserDeposits.length - 1].newCurrentDepositQueueHash;

        // Set up mock: TempoState will return this hash when reading from portal
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, newProcessedHash
        );

        // Mock precompiles
        if (shouldSucceed) {
            _setupPrecompileMocksSuccess(decryptedTo, decryptedMemo);
        } else {
            _setupPrecompileMocksFail();
        }

        // Process on zone via advanceTempo
        vm.prank(address(0));
        l2Inbox.advanceTempo("", queued, decs, new EnabledToken[](0));

        // Clear pending
        delete pendingUserDeposits;

        // Update zone block hash (simulated)
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "enc-deposits", newProcessedHash));
    }

    /*//////////////////////////////////////////////////////////////
            ENCRYPTED DEPOSIT INTEGRATION TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Full lifecycle: encrypted deposit on L1 → relay to zone → mint to decrypted recipient
    function test_fullFlow_depositAndMint() public {
        // === STEP 1: Sequencer sets encryption key on L1 ===
        (bytes32 encKeyX, uint8 encKeyYParity) = _setEncKeyOnL1(ENC_KEY_1);

        // === STEP 2: Alice makes encrypted deposit on L1 ===
        uint128 depositAmount = 1000e6;
        uint128 fee = l1Portal.calculateDepositFee();
        uint128 netAmount = depositAmount - fee;
        DepositPayload memory payload = _makeDepositPayload();

        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        bytes32 l1DepositHash =
            l1Portal.deposit(address(l2ZoneToken), depositAmount, 0, payload, alice);
        vm.stopPrank();

        // Verify L1 state
        assertEq(l1Portal.currentDepositQueueHash(), l1DepositHash, "L1 queue hash mismatch");
        assertEq(
            l2ZoneToken.balanceOf(address(l1Portal)),
            depositAmount - fee,
            "Portal should hold net amount"
        );

        // === STEP 3: Sequencer observes encrypted deposit event ===
        _sequencerObserveDeposit(alice, netAmount, 0, payload);

        // Verify our local hash matches L1
        assertEq(
            pendingUserDeposits[0].newCurrentDepositQueueHash,
            l1DepositHash,
            "Observed hash must match L1 hash"
        );

        // === STEP 4: Set up zone-side encryption key mock and relay ===
        _setupEncryptionKeyMockOnZone(0, encKeyX, encKeyYParity);

        address decryptedRecipient = bob;
        bytes32 decryptedMemo = bytes32("secret memo");
        bytes32 newProcessedHash =
            _sequencerRelayDepositsToL2(decryptedRecipient, decryptedMemo, true);

        // Verify zone state — tokens minted to decrypted recipient (bob starts with 100K)
        assertEq(
            l2ZoneToken.balanceOf(decryptedRecipient),
            100_000e6 + netAmount,
            "Recipient should receive tokens"
        );
        assertEq(
            l2ZoneToken.balanceOf(alice),
            100_000e6 - depositAmount,
            "Sender keeps remaining balance"
        );
        assertEq(
            l2Inbox.processedDepositQueueHash(), newProcessedHash, "Zone processed hash mismatch"
        );

        // === STEP 5: Submit batch to L1 ===
        _sequencerSubmitBatch(newProcessedHash);

        // Verify L1 batch state updated
        assertEq(l1Portal.withdrawalBatchIndex(), 1, "Batch index should advance");
        assertEq(l1Portal.blockHash(), l2BlockHash, "Block hash should update");
    }

    /// @notice Full lifecycle: encrypted deposit → decryption failure → funds returned to sender
    function test_fullFlow_depositBounce() public {
        // === STEP 1: Sequencer sets encryption key on L1 ===
        (bytes32 encKeyX, uint8 encKeyYParity) = _setEncKeyOnL1(ENC_KEY_1);

        // === STEP 2: Alice makes encrypted deposit on L1 ===
        uint128 depositAmount = 1000e6;
        uint128 fee = l1Portal.calculateDepositFee();
        uint128 bouncebackFee = l1Portal.calculateBouncebackFee();
        uint128 netAmount = depositAmount - fee;
        DepositPayload memory payload = _makeDepositPayload();

        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        l1Portal.deposit(address(l2ZoneToken), depositAmount, 0, payload, alice);
        vm.stopPrank();

        // === STEP 3: Sequencer observes and relays with FAILED decryption ===
        _sequencerObserveDeposit(alice, netAmount, 0, payload);
        _setupEncryptionKeyMockOnZone(0, encKeyX, encKeyYParity);

        // Even with shouldSucceed=false, we still call the relay helper
        bytes32 newProcessedHash =
            _sequencerRelayDepositsToL2(address(0xBEEF), bytes32("wrong"), false);

        // Verify zone state — no mint was attempted, and a Tempo refund withdrawal was enqueued.
        assertEq(
            l2ZoneToken.balanceOf(alice),
            100_000e6 - depositAmount,
            "Sender should not receive a zone mint"
        );
        assertEq(l2ZoneToken.balanceOf(address(0xBEEF)), 0, "Failed recipient should get nothing");
        assertEq(l2Outbox.pendingWithdrawalsCount(), 1, "Bounce-back withdrawal should be queued");
        assertEq(
            l2Inbox.processedDepositQueueHash(), newProcessedHash, "Zone processed hash mismatch"
        );

        // === STEP 4: Submit batch to L1 with the deposit bounce-back withdrawal ===
        uint256 aliceBeforeRefund = l2ZoneToken.balanceOf(alice);
        uint256 portalBeforeRefund = l2ZoneToken.balanceOf(address(l1Portal));
        _sequencerSubmitBatch(newProcessedHash);
        assertEq(l1Portal.withdrawalBatchIndex(), 1, "Batch index should advance");

        Withdrawal memory bounce = Withdrawal({
            token: address(l2ZoneToken),
            senderTag: keccak256(abi.encodePacked(address(0), bytes32(0))),
            to: alice,
            amount: netAmount,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackNonce: 0,
            callbackData: "",
            encryptedSender: ""
        });
        bytes32 expectedQueueHash = keccak256(abi.encode(bounce, EMPTY_SENTINEL));
        assertEq(l1Portal.withdrawalQueueSlot(0), expectedQueueHash);

        l1Portal.processWithdrawals(_singleWithdrawal(bounce), bytes32(0));
        assertEq(l2ZoneToken.balanceOf(alice), aliceBeforeRefund + netAmount - bouncebackFee);
        assertEq(l2ZoneToken.balanceOf(address(l1Portal)), portalBeforeRefund - netAmount);
    }

    /// @notice Key rotation: two encrypted deposits using different encryption keys
    function test_fullFlow_keyRotationWithPendingDeposits() public {
        // === STEP 1: Sequencer sets first encryption key ===
        (bytes32 keyX1, uint8 keyYParity1) = _setEncKeyOnL1(ENC_KEY_1);

        // === STEP 2: Alice deposits with keyIndex=0 ===
        uint128 depositAmount = 1000e6;
        uint128 fee = l1Portal.calculateDepositFee();
        uint128 bouncebackFee = l1Portal.calculateBouncebackFee();
        uint128 netAmount = depositAmount - fee;
        DepositPayload memory payload1 = _makeDepositPayload();

        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        bytes32 h1 = l1Portal.deposit(address(l2ZoneToken), depositAmount, 0, payload1, alice);
        vm.stopPrank();

        // === STEP 3: Sequencer rotates to second encryption key ===
        vm.roll(block.number + 100);
        (bytes32 keyX2, uint8 keyYParity2) = _setEncKeyOnL1(ENC_KEY_2);

        // === STEP 4: Bob deposits with keyIndex=1 ===
        DepositPayload memory payload2 = _makeDepositPayload();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        bytes32 h2 = l1Portal.deposit(address(l2ZoneToken), depositAmount, 1, payload2, bob);
        vm.stopPrank();

        assertEq(l1Portal.currentDepositQueueHash(), h2, "L1 hash after both deposits");

        // === STEP 5: Compute expected hashes ===
        bytes32 prevHash = l2Inbox.processedDepositQueueHash();
        Deposit memory ed1 = Deposit({
            token: address(l2ZoneToken),
            sender: alice,
            amount: netAmount,
            tempoRefundRecipient: alice,
            keyIndex: 0,
            encrypted: payload1
        });
        bytes32 hash1 = keccak256(abi.encode(DepositType.Deposit, ed1, prevHash));
        assertEq(hash1, h1, "hash1 must match L1");

        Deposit memory ed2 = Deposit({
            token: address(l2ZoneToken),
            sender: bob,
            amount: netAmount,
            tempoRefundRecipient: bob,
            keyIndex: 1,
            encrypted: payload2
        });
        bytes32 hash2 = keccak256(abi.encode(DepositType.Deposit, ed2, hash1));
        assertEq(hash2, h2, "hash2 must match L1");

        // === STEP 6: Build queue and relay ===
        QueuedDeposit[] memory queued = new QueuedDeposit[](2);
        queued[0] = QueuedDeposit({
            depositType: DepositType.Deposit, depositData: abi.encode(ed1), rejected: false
        });
        queued[1] = QueuedDeposit({
            depositType: DepositType.Deposit, depositData: abi.encode(ed2), rejected: false
        });

        address aliceRecipient = address(0x700);
        bytes32 aliceMemo = bytes32("alice-secret");
        address bobRecipient = address(0x800);
        bytes32 bobMemo = bytes32("bob-secret");

        DecryptionData[] memory decs = new DecryptionData[](2);
        decs[0] = DecryptionData({
            sharedSecret: bytes32(uint256(0xDEAD)),
            sharedSecretYParity: 0x02,
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });
        decs[1] = DecryptionData({
            sharedSecret: bytes32(uint256(0xBEEF)),
            sharedSecretYParity: 0x02,
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(3)), c: bytes32(uint256(4)) })
        });

        // Set up zone-side mocks: both keys in storage
        _setupEncryptionKeyMockOnZone(0, keyX1, keyYParity1);
        _setupEncryptionKeyMockOnZone(1, keyX2, keyYParity2);

        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, hash2
        );

        // Mock precompiles to return the same plaintext for both deposits.
        // Since vm.mockCall with just the selector matches ALL calls, both encrypted
        // deposits will decrypt to the same (sharedRecipient, sharedMemo).
        address sharedRecipient = address(0x700);
        bytes32 sharedMemo = bytes32("shared-secret");

        vm.etch(CHAUM_PEDERSEN_VERIFY, hex"00");
        vm.etch(AES_GCM_DECRYPT, hex"00");
        vm.mockCall(
            CHAUM_PEDERSEN_VERIFY,
            abi.encodeWithSelector(IChaumPedersenVerify.verifyProof.selector),
            abi.encode(true)
        );
        bytes memory plaintext = EncryptedDepositLib.encodePlaintext(sharedRecipient, sharedMemo);
        vm.mockCall(
            AES_GCM_DECRYPT,
            abi.encodeWithSelector(IAesGcmDecrypt.decrypt.selector),
            abi.encode(plaintext, true)
        );

        vm.prank(address(0));
        l2Inbox.advanceTempo("", queued, decs, new EnabledToken[](0));

        // === STEP 7: Verify ===
        // Both deposits go to sharedRecipient (no prior balance)
        assertEq(
            l2ZoneToken.balanceOf(sharedRecipient),
            netAmount * 2,
            "Recipient should receive both deposits"
        );
        // alice/bob: 100K - deposit, no zone mint to them (encrypted goes to sharedRecipient)
        assertEq(
            l2ZoneToken.balanceOf(alice), 100_000e6 - depositAmount, "Alice keeps remaining balance"
        );
        assertEq(
            l2ZoneToken.balanceOf(bob), 100_000e6 - depositAmount, "Bob keeps remaining balance"
        );
        assertEq(l2Inbox.processedDepositQueueHash(), hash2, "Zone processed hash matches L1");

        // === STEP 8: Submit batch ===
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "key-rotation", hash2));
        _sequencerSubmitBatch(hash2);
        assertEq(l1Portal.withdrawalBatchIndex(), 1, "Batch index should advance");
    }

}
