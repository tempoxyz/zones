// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    BlockTransition,
    Capability,
    Deposit,
    DepositPayload,
    DepositQueueTransition,
    DepositType,
    ENCRYPTION_KEY_GRACE_PERIOD,
    EncryptionKeyEntry,
    FastBarrierResolution,
    FastBarrierStatement,
    FastCheckpointStatement,
    FastEpochConfig,
    FastPeerBarrier,
    FastProofMode,
    IVerifier,
    IZoneFactory,
    IZoneMessenger,
    IZonePortal,
    MAX_WITHDRAWAL_CALLBACK_GAS,
    QueuedDeposit,
    Role,
    TokenConfig,
    TokenEnablementTransition,
    Withdrawal,
    WithdrawalBounceBackDeposit,
    ZONE_FACTORY_ADDRESS,
    ZONE_PORTAL_IMPL_ADDRESS
} from "../interfaces/IZone.sol";
import {getBlockHash} from "../libraries/BlockHashHistory.sol";
import {DepositQueueLib} from "../libraries/DepositQueueLib.sol";
import {ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE} from "../libraries/EncryptedDeposit.sol";
import {Secp256k1Lib} from "../libraries/Secp256k1Lib.sol";
import {WithdrawalQueue, WithdrawalQueueLib} from "../libraries/WithdrawalQueueLib.sol";
import {StdPrecompiles} from "tempo-std/StdPrecompiles.sol";
import {ITIP20} from "tempo-std/interfaces/ITIP20.sol";
import {ITIP20Factory} from "tempo-std/interfaces/ITIP20Factory.sol";
import {ITIP403Registry} from "tempo-std/interfaces/ITIP403Registry.sol";

/// @title ZonePortal
/// @notice Per-zone portal that escrows zone tokens on Tempo and manages deposits/withdrawals
contract ZonePortal is IZonePortal {
    using WithdrawalQueueLib for WithdrawalQueue;

    /*//////////////////////////////////////////////////////////////
                               CONSTANTS
    //////////////////////////////////////////////////////////////*/

    /// @notice TIP-403 registry for transfer policy authorization checks
    ITIP403Registry internal constant TIP403_REGISTRY = ITIP403Registry(StdPrecompiles.TIP403_REGISTRY_ADDRESS);

    /// @notice Fixed gas value for deposit fee calculation
    /// @dev Set to 100,000 gas. Deposit fee = FIXED_DEPOSIT_GAS * zoneGasRate.
    ///      This provides a stable pricing basis for deposits while allowing the admin
    ///      to adjust the zoneGasRate based on operational costs.
    uint64 public constant FIXED_DEPOSIT_GAS = 100_000;

    /// @notice Maximum deposits that may be appended to this portal in one Tempo block.
    /// @dev Under T9, processing 230 encrypted deposits rejected by the issuer's
    ///      TIP-403 transfer policy uses 193,044,874 gas, leaving 6,955,126 gas
    ///      below the buffered 200,000,000 gas ceiling.
    uint64 public constant MAX_UNPROCESSED_DEPOSITS = 230;

    /// @notice Maximum enabled tokens that may remain unprocessed by the Zone.
    /// @dev Under T9, processing 230 worst-case deposits plus 8 token enablements with maximum
    ///      metadata uses 214,832,282 gas, below the buffered 225,000,000 gas ceiling.
    uint64 public constant MAX_UNPROCESSED_TOKEN_ENABLEMENTS = 8;

    /// @notice Maximum byte length of each token metadata string copied into the zone.
    /// @dev Keeps name, symbol, and currency in Solidity's one-slot short-string representation.
    uint256 public constant MAX_TOKEN_METADATA_BYTES = 31;

    /// @dev Reserves deposit-queue capacity for one maximum-size withdrawal batch.
    ///      Each withdrawal can append at most one deposit: either a callback-triggered
    ///      deposit on success or a bounce-back deposit on failure.
    ///      The 20M batch gas ceiling fits at most 19 withdrawals, plus one slot of margin.
    uint64 internal constant WITHDRAWAL_PROCESSING_DEPOSIT_RESERVE = 20;

    uint256 internal constant WITHDRAWAL_NOT_ENTERED = 0;
    uint256 internal constant WITHDRAWAL_PROCESSING = 1;
    uint256 internal constant CALLBACK_DEPOSIT_AVAILABLE = 2;
    uint256 internal constant CALLBACK_DEPOSIT_CONSUMED = 3;

    /// @notice Scale factor from 18-decimal Tempo gas prices to 6-decimal TIP-20 units
    uint256 internal constant TEMPO_BASE_FEE_SCALE = 1e12;

    /// @notice Maximum gas a withdrawal callback may request
    /// @dev Over-cap legacy withdrawals are dequeued and bounced back in `processWithdrawals`.
    uint64 public constant MAX_WITHDRAWAL_GAS_LIMIT = MAX_WITHDRAWAL_CALLBACK_GAS;

    /// @notice Maximum allowed gas fee rate to prevent overflows
    uint128 public constant MAX_GAS_FEE_RATE = 1e18;

    /// @notice Maximum number of independently countable settlement signers.
    /// @dev Matches the creation and replacement bound fixed by TIP-1091.
    uint256 public constant MAX_SEQUENCERS = 8;

    /// @notice Duration of every emergency pause.
    uint64 public constant PAUSE_DURATION = 30 days;

    /// @notice Delay before a capability abdication becomes effective.
    uint64 public constant ABDICATION_DELAY = PAUSE_DURATION;

    bytes32 internal constant EIP712_DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    bytes32 internal constant NAME_HASH = keccak256("ZonePortal");
    bytes32 internal constant VERSION_HASH = keccak256("1");
    bytes32 internal constant SETTLEMENT_ATTESTATION_TYPEHASH = keccak256(
        "SettlementAttestation(uint32 zoneId,uint64 sequencerSetVersion,uint256 zoneHeight,uint256 withdrawalBatchIndex,address verifier,uint64 tempoBlockNumber,uint64 anchorBlockNumber,bytes32 anchorBlockHash,bytes32 blockTransitionHash,bytes32 depositQueueTransitionHash,bytes32 tokenEnablementTransitionHash,bytes32 withdrawalQueueHash,bytes32 verifierConfigHash)"
    );
    bytes32 internal constant FAST_SETTLEMENT_ATTESTATION_TYPEHASH = keccak256(
        "FastSettlementAttestation(uint32 zoneId,uint64 fastEpoch,bytes32 rosterHash,uint256 previousZoneHeight,bytes32 previousBlockHash,uint64 previousWithdrawalBatchIndex,uint256 zoneHeight,uint256 withdrawalBatchIndex,address verifier,uint64 tempoBlockNumber,uint64 anchorBlockNumber,bytes32 anchorBlockHash,bytes32 blockTransitionHash,bytes32 depositQueueTransitionHash,bytes32 tokenEnablementTransitionHash,bytes32 withdrawalQueueHash,bytes32 verifierConfigHash)"
    );
    bytes32 internal constant FAST_BARRIER_DOMAIN = keccak256("TEMPO_ZONE_FAST_BARRIER_T14_V1");
    bytes32 internal constant FAST_BARRIER_RESOLUTION_DOMAIN =
        keccak256("TEMPO_ZONE_FAST_BARRIER_RESOLUTION_T14_V1");
    bytes32 internal constant FAST_FINAL_SETTLEMENT_DOMAIN =
        keccak256("TEMPO_ZONE_FAST_FINAL_SETTLEMENT_T14_V1");
    bytes32 internal constant FAST_CHECKPOINT_DOMAIN = keccak256("TEMPO_ZONE_FAST_CHECKPOINT_T14_V1");
    bytes32 internal constant FAST_BARRIERS_DOMAIN = keccak256("TEMPO_ZONE_FAST_BARRIERS_T14_V1");
    bytes32 public constant FAST_EMPTY_UNRESOLVED_ROOT =
        keccak256("TEMPO_ZONE_FAST_EMPTY_UNRESOLVED_T14_V1");
    bytes32 public constant T13_PROTOTYPE_VERIFIER_CODE_HASH =
        0xcf7b19d3c186e4fd235c94907d10bba5c1ce21d6e815676b00aaf298b456de14;
    bytes32 internal constant DEVELOPMENT_PROTOTYPE_VERIFIER_CODE_HASH =
        0xc6bc17dc6724fb475ce3c59ec94e01bd733c2996b41bc82281b4d638b928cc33;

    /// @notice Cross-component compatibility pin for the fast protocol.
    bytes32 public constant FAST_PROTOCOL_NATIVE_PIN = keccak256("TEMPO_ZONE_FAST_PROTOCOL_T14_V1");
    /// @notice Ten-Zone topology requires a closure barrier from each of the other nine Zones.
    uint16 public constant FAST_EXPECTED_PEER_BARRIERS = 9;
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice Governance admin address
    address public admin;

    /// @notice Zone gas rate (zone token units per gas unit on the zone)
    /// @dev Sequencer publishes this rate and takes the risk on zone gas costs.
    ///      Deposit fee = FIXED_DEPOSIT_GAS * zoneGasRate
    uint128 public zoneGasRate;
    uint64 public withdrawalBatchIndex;
    bytes32 public blockHash;

    /// @notice Current deposit queue hash (where new deposits land)
    bytes32 public currentDepositQueueHash;

    /// @notice Total number of deposits enqueued (monotonic counter, 1-indexed).
    /// @dev Each deposit(), depositEncrypted(), and withdrawal bounce-back increments this.
    ///      The deposit number is emitted in deposit events so users can track their position.
    uint64 public depositCount;

    /// @notice Last deposit number confirmed as processed by a batch proof.
    /// @dev Updated in submitBatch(). A deposit with number N is confirmed once
    ///      lastProcessedDepositNumber >= N.
    uint64 public lastProcessedDepositNumber;

    /// @notice Last Tempo block number the zone has synced to
    uint64 public lastSyncedTempoBlockNumber;

    /// @notice Gas amount used to price a failed-deposit bounce-back on Tempo.
    /// @dev Packed into the unused bytes in slot 4. Defaults to zero.
    uint64 public bouncebackGas;

    /// @notice Historical encryption public keys with activation blocks
    /// @dev Users specify which key they encrypted to (by index). Maintained for key rotation.
    ///      Stored at slot 5 in the ZonePortal storage layout.
    EncryptionKeyEntry[] internal _encryptionKeys;

    /// @notice Per-token configuration (stored at slot 6)
    /// @dev TokenConfig.enabled is permanent (write-once true); depositsActive can be toggled.
    mapping(address => TokenConfig) internal _tokenConfigs;

    /// @notice Append-only list of enabled tokens (stored at slot 7)
    /// @dev Tokens can never be removed from this list (non-custodial guarantee).
    address[] internal _enabledTokens;

    /// @notice Refunds parked after a deposit bounce-back transfer reverts on Tempo.
    mapping(address token => mapping(address owner => uint128 amount)) public refunds;

    /// @notice Withdrawal queue (zone→Tempo): unbounded FIFO
    WithdrawalQueue internal _withdrawalQueue;

    /// @notice Operator RPC endpoint for the zone
    string public rpcUrl;

    /// @notice Pending admin for two-step admin transfer
    address public pendingAdmin;

    /// @notice Reentrancy guard for withdrawal delivery.
    uint256 internal _withdrawalReentrancyStatus;

    /// @notice Zone metadata stored after the cross-domain layout.
    /// @dev These values must remain in account storage so each delegatecall proxy observes its
    ///      own metadata. Keep them after the established slots read directly by zone contracts.
    uint32 public zoneId;
    /// @notice Fixed callback messenger assigned during initialization.
    address public messenger;
    address public verifier;
    bool internal _initialized;

    /// @notice Configuration nonce for the active sequencer set and threshold.
    uint64 public sequencerSetVersion;
    uint8 public sequencerThreshold;
    uint256 public zoneHeight;
    address[] internal _sequencers;
    /// @dev Reserved slot 19, available for future use.
    uint256 private _reservedSlot19;
    /// @dev Mutually exclusive Portal roles. Sequencer membership is derived from this mapping.
    mapping(address => Role) internal role;

    /// @dev Solidity packs both enforcement booleans into slot 21.
    bool internal _isAccessEnforced;
    bool internal _isGatewayEnforced;

    /// @dev Reserve the remainder of slot 21 so the cross-domain fee cap has a dedicated slot.
    uint240 private _enforcementModesPadding;

    /// @notice Maximum Tempo gas rate a sequencer may configure on the zone-side outbox.
    /// @dev Defaults to zero and is read from finalized Tempo state by zone system contracts.
    uint128 public maxTempoGasRate;

    /// @notice Individual sequencer address of the active block-producing leader.
    /// @dev Appended after maxTempoGasRate; do not reorder existing storage. Zone nodes derive
    ///      leadership exclusively from finalized reads of these fields and the LeaderUpdated
    ///      event. Reads as zero for portals initialized before leadership landed; the first
    ///      setLeader from that state bootstraps epoch 1.
    address public leader;

    /// @notice Monotonic fencing epoch, incremented exactly once per real leader change.
    uint64 public leaderEpoch;

    /// @notice Tempo block number that recorded the most recent leader transition.
    uint64 public leaderActivationTempoBlock;

    /// @dev Per-Tempo-block deposit admission counter. Appended for upgrade-safe storage layout.
    uint64 internal _depositCountBlock;
    uint64 internal _depositsInCurrentBlock;

    /// @dev Retired per-Tempo-block token-enablement counters, retained for storage compatibility.
    uint64 internal _tokenEnableCountBlock;
    uint64 internal _tokensEnabledInCurrentBlock;

    /// @notice Timestamp at which the current emergency pause expires.
    /// @dev Packed after the token-enablement counter in slot 25.
    uint64 public pauseExpiry;

    /// @notice Append-only commitment to every enabled token and its metadata.
    /// @dev Stored at slot 26.
    bytes32 public tokenEnablementHash;

    /// @notice Time after which the corresponding configuration surface is permanently closed.
    mapping(Capability => uint64) public abdicationEffectiveAt;

    /// @notice Enabled-token prefix confirmed by accepted Zone proofs.
    /// @dev Appended after all T10 storage for upgrade safety.
    uint64 public lastProcessedEnabledTokenCount;

    /// @notice Whether the T13 token cursor has been authenticated by an operational batch.
    bool public tokenEnablementCursorInitialized;

    /// @notice Current fast authority epoch. Zero means no epoch has ever been configured.
    uint64 public fastEpoch;
    mapping(uint64 epoch => FastEpochConfig config) internal _fastEpochs;
    mapping(uint64 epoch => address[] members) internal _fastEpochMembers;
    mapping(uint64 epoch => mapping(address member => bool)) internal _isFastEpochMember;
    mapping(uint64 epoch => address[] peerPortals) internal _fastEpochPeers;
    mapping(uint64 epoch => mapping(address peerPortal => bool)) internal _isFastEpochPeer;
    mapping(uint64 epoch => mapping(address peerPortal => FastPeerBarrier barrier)) internal _fastPeerBarriers;

    /*//////////////////////////////////////////////////////////////
                             INITIALIZATION
    //////////////////////////////////////////////////////////////*/

    function initialize(
        uint32 _zoneId,
        address _initialToken,
        bool accessEnforced,
        bool gatewayEnforced,
        address[] calldata _allowedAccounts,
        address[] calldata _zoneGateways,
        address _messenger,
        address _admin,
        address[] calldata initialSequencers,
        uint8 _threshold,
        address _verifier,
        string calldata _rpcUrl
    ) external onlyDelegateCall {
        if (msg.sender != ZONE_FACTORY_ADDRESS) revert NotFactory();
        if (_initialized) revert AlreadyInitialized();

        _initialized = true;
        zoneId = _zoneId;
        messenger = _messenger;
        admin = _admin;
        verifier = _verifier;
        _isAccessEnforced = accessEnforced;
        _isGatewayEnforced = gatewayEnforced;
        rpcUrl = _rpcUrl;
        emit EnforcementModesUpdated(accessEnforced, gatewayEnforced);

        _replaceSequencerSet(initialSequencers, _threshold, false);
        // The first sequencer bootstraps leadership so a fresh zone has a producer without a
        // separate setLeader call. The creation block is replayed by every zone node because
        // zone genesis anchors before createZone.
        _setLeader(initialSequencers[0]);

        for (uint256 i; i < _zoneGateways.length; ++i) {
            address account = _zoneGateways[i];
            require(role[account] == Role.None);
            role[account] = Role.CallbackGateway;
            emit RoleUpdated(account, Role.None, Role.CallbackGateway);
        }
        for (uint256 i; i < _allowedAccounts.length; ++i) {
            address account = _allowedAccounts[i];
            require(account != _messenger);
            require(role[account] == Role.None);
            role[account] = Role.Account;
            emit RoleUpdated(account, Role.None, Role.Account);
        }

        // Enable the initial token. Operational token enablement remains frozen until a Zone batch
        // authenticates a nonzero enabled-token prefix and initializes the cursor.
        _enableTokenInternal(_initialToken);
    }

    /*//////////////////////////////////////////////////////////////
                               MODIFIERS
    //////////////////////////////////////////////////////////////*/

    /// @dev Initialization is valid only in a portal proxy's storage context.
    modifier onlyDelegateCall() {
        if (address(this) == ZONE_PORTAL_IMPL_ADDRESS) revert MustDelegateCall();
        _;
    }

    modifier onlySequencer() {
        if (!isSequencer(msg.sender)) revert NotSequencer();
        _;
    }

    modifier onlySequencerOrAdmin() {
        if (msg.sender != admin && !isSequencer(msg.sender)) revert NotSequencer();
        _;
    }

    modifier onlyAdmin() {
        if (msg.sender != admin) revert NotAdmin();
        _;
    }

    modifier whenNotPaused() {
        if (paused()) revert PortalIsPaused();
        _;
    }

    modifier onlySelf() {
        if (msg.sender != address(this)) revert NotSelf();
        _;
    }

    modifier nonReentrantWithdrawal() {
        if (_withdrawalReentrancyStatus != WITHDRAWAL_NOT_ENTERED) {
            revert ReentrantWithdrawal();
        }
        _withdrawalReentrancyStatus = WITHDRAWAL_PROCESSING;
        _;
        _withdrawalReentrancyStatus = WITHDRAWAL_NOT_ENTERED;
    }

    /// @inheritdoc IZonePortal
    function setSequencerSet(address[] calldata newSequencers, uint8 newThreshold) external onlyAdmin {
        _rejectLegacyAuthorityChangeDuringFastEpoch();
        _replaceSequencerSet(newSequencers, newThreshold, true);
    }

    function _replaceSequencerSet(address[] calldata newSequencers, uint8 newThreshold, bool rejectUnchanged) internal {
        uint256 length = newSequencers.length;
        if (length == 0 || length > MAX_SEQUENCERS || newThreshold == 0 || newThreshold > length) {
            revert InvalidSequencerSet();
        }

        for (uint256 i = 0; i < length; ++i) {
            address signer = newSequencers[i];
            if (signer == address(0)) revert InvalidSequencerSet();
            Role existing = role[signer];
            require(existing == Role.None || existing == Role.Sequencer);

            for (uint256 j = 0; j < i; ++j) {
                if (newSequencers[j] == signer) revert InvalidSequencerSet();
            }
        }

        bool membersUnchanged = length == _sequencers.length;
        if (membersUnchanged) {
            for (uint256 i = 0; i < length; ++i) {
                if (!isSequencer(newSequencers[i])) {
                    membersUnchanged = false;
                    break;
                }
            }
        }
        if (rejectUnchanged && membersUnchanged && newThreshold == sequencerThreshold) {
            revert SequencerConfigurationUnchanged();
        }

        for (uint256 i = 0; i < _sequencers.length; ++i) {
            address signer = _sequencers[i];
            role[signer] = Role.None;
            emit RoleUpdated(signer, Role.Sequencer, Role.None);
        }
        delete _sequencers;
        for (uint256 i = 0; i < length; ++i) {
            address signer = newSequencers[i];
            _sequencers.push(signer);
            role[signer] = Role.Sequencer;
            emit RoleUpdated(signer, Role.None, Role.Sequencer);
        }
        // Rotating out the active leader would strand block production: transfer leadership
        // first (add the replacement, setLeader, then remove the old member).
        if (leader != address(0) && !isSequencer(leader)) {
            revert ActiveLeaderRemoved();
        }

        sequencerThreshold = newThreshold;
        uint64 nonce = sequencerSetVersion;
        if (rejectUnchanged) nonce = ++sequencerSetVersion;
        emit SequencerSetUpdated(nonce, newThreshold, newSequencers);
    }

    /// @inheritdoc IZonePortal
    function sequencerCount() external view returns (uint256) {
        return _sequencers.length;
    }

    /// @inheritdoc IZonePortal
    function sequencerAt(uint256 index) external view returns (address) {
        return _sequencers[index];
    }

    /// @inheritdoc IZonePortal
    function isSequencer(address account) public view returns (bool) {
        return role[account] == Role.Sequencer;
    }

    /// @inheritdoc IZonePortal
    function setLeader(address newLeader, uint64 expectedEpoch) external onlySequencerOrAdmin {
        _rejectLegacyAuthorityChangeDuringFastEpoch();
        if (!isSequencer(newLeader)) revert InvalidLeader();
        // Idempotent fanout: every node relays the same target, only the first call transitions.
        if (newLeader == leader) return;
        // Compare-and-set: a delayed duplicate carrying a pre-handoff epoch cannot roll
        // leadership back after a later transition.
        if (leaderEpoch != expectedEpoch) {
            revert StaleLeadershipEpoch(expectedEpoch, leaderEpoch);
        }
        // One distinct leader per Tempo block keeps exactly one authorized producer for the
        // corresponding zone block.
        if (leaderActivationTempoBlock == uint64(block.number)) {
            revert LeaderAlreadyUpdatedThisBlock();
        }

        _setLeader(newLeader);
    }

    /// @inheritdoc IZonePortal
    function fastEpochActive() public view returns (bool) {
        uint64 epoch = fastEpoch;
        return epoch != 0 && !_fastEpochs[epoch].retired;
    }

    /// @inheritdoc IZonePortal
    function fastEpochConfig(uint64 epoch) external view returns (FastEpochConfig memory) {
        return _fastEpochs[epoch];
    }

    /// @inheritdoc IZonePortal
    function fastEpochMemberCount(uint64 epoch) external view returns (uint256) {
        return _fastEpochMembers[epoch].length;
    }

    /// @inheritdoc IZonePortal
    function fastEpochMemberAt(uint64 epoch, uint256 index) external view returns (address) {
        return _fastEpochMembers[epoch][index];
    }

    /// @inheritdoc IZonePortal
    function isFastEpochMember(uint64 epoch, address account) external view returns (bool) {
        return _isFastEpochMember[epoch][account];
    }

    /// @inheritdoc IZonePortal
    function fastEpochPeerCount(uint64 epoch) external view returns (uint256) {
        return _fastEpochPeers[epoch].length;
    }

    /// @inheritdoc IZonePortal
    function fastEpochPeerAt(uint64 epoch, uint256 index) external view returns (address) {
        return _fastEpochPeers[epoch][index];
    }

    /// @inheritdoc IZonePortal
    function isFastEpochPeer(uint64 epoch, address peerPortal) external view returns (bool) {
        return _isFastEpochPeer[epoch][peerPortal];
    }

    /// @inheritdoc IZonePortal
    function fastPeerBarrier(uint64 epoch, address peerPortal) external view returns (FastPeerBarrier memory) {
        return _fastPeerBarriers[epoch][peerPortal];
    }

    /// @inheritdoc IZonePortal
    function configureFastEpoch(
        uint64 epoch,
        uint32 protocolVersion,
        FastProofMode proofMode,
        bytes32 expectedVerifierCodeHash,
        bytes32 expectedVerifierConfigHash,
        address[] calldata members,
        address[] calldata peerPortals,
        bytes32 rosterHash
    ) external onlyDelegateCall {
        if (msg.sender != ZONE_FACTORY_ADDRESS) revert NotFactory();
        if (
            epoch == 0 || epoch <= fastEpoch || protocolVersion == 0 || proofMode == FastProofMode.Unset
                || expectedVerifierCodeHash == bytes32(0) || expectedVerifierConfigHash == bytes32(0)
                || members.length != 3 || peerPortals.length != FAST_EXPECTED_PEER_BARRIERS
        ) revert InvalidFastEpoch();
        if (
            verifier.codehash != expectedVerifierCodeHash
                || (proofMode == FastProofMode.ProofRequired
                    && (expectedVerifierCodeHash == T13_PROTOTYPE_VERIFIER_CODE_HASH
                        || expectedVerifierCodeHash == DEVELOPMENT_PROTOTYPE_VERIFIER_CODE_HASH
                        || _verifierAcceptsInvalidProof()))
        ) revert InvalidFastProofConfiguration();

        bytes32 expectedRosterHash = keccak256(
            abi.encode(
                keccak256("TEMPO_ZONE_FAST_ROSTER_T14_V1"),
                address(this),
                epoch,
                protocolVersion,
                uint8(2),
                proofMode,
                expectedVerifierCodeHash,
                expectedVerifierConfigHash,
                members,
                peerPortals
            )
        );
        if (rosterHash != expectedRosterHash) revert InvalidFastEpoch();
        bytes32 peersHash = keccak256(abi.encode(peerPortals));

        uint64 previous = fastEpoch;
        if (previous != 0) {
            FastEpochConfig storage prior = _fastEpochs[previous];
            if (!prior.retired) revert FastEpochActive(previous);
            if (
                prior.nextEpoch != epoch || prior.nextRosterHash != rosterHash || prior.checkpointHash == bytes32(0)
            ) revert InvalidFastCertificate();
        }

        for (uint256 i; i < members.length; ++i) {
            address member = members[i];
            if (member == address(0)) revert InvalidFastEpoch();
            for (uint256 j; j < i; ++j) {
                if (members[j] == member) revert InvalidFastEpoch();
            }
            _fastEpochMembers[epoch].push(member);
            _isFastEpochMember[epoch][member] = true;
        }
        for (uint256 i; i < peerPortals.length; ++i) {
            address peer = peerPortals[i];
            if (peer == address(0) || peer == address(this) || !IZoneFactory(ZONE_FACTORY_ADDRESS).isZonePortal(peer)) {
                revert InvalidFastEpoch();
            }
            for (uint256 j; j < i; ++j) {
                if (peerPortals[j] == peer) revert InvalidFastEpoch();
            }
            _fastEpochPeers[epoch].push(peer);
            _isFastEpochPeer[epoch][peer] = true;
        }

        _fastEpochs[epoch] = FastEpochConfig({
            protocolVersion: protocolVersion,
            threshold: 2,
            proofMode: proofMode,
            closed: false,
            retired: false,
            expectedPeerBarriers: FAST_EXPECTED_PEER_BARRIERS,
            recordedPeerBarriers: 0,
            finalizedPeerBarriers: 0,
            activatedAtTempoBlock: uint64(block.number),
            rosterHash: rosterHash,
            peersHash: peersHash,
            expectedVerifierCodeHash: expectedVerifierCodeHash,
            expectedVerifierConfigHash: expectedVerifierConfigHash,
            closureHash: bytes32(0),
            finalSettlementHeight: 0,
            finalSettlementBlockHash: bytes32(0),
            finalSettlementWithdrawalBatchIndex: 0,
            barriersHash: bytes32(0),
            finalSettlementHash: bytes32(0),
            nextEpoch: 0,
            nextRosterHash: bytes32(0),
            checkpointLogTerm: 0,
            checkpointLogIndex: 0,
            checkpointHeight: 0,
            checkpointBlockHash: bytes32(0),
            checkpointStateRoot: bytes32(0),
            checkpointHash: bytes32(0)
        });
        fastEpoch = epoch;
        emit FastEpochActivated(
            epoch,
            protocolVersion,
            rosterHash,
            peersHash,
            proofMode,
            expectedVerifierCodeHash,
            expectedVerifierConfigHash,
            members,
            peerPortals
        );
    }

    /// @inheritdoc IZonePortal
    function closeFastEpoch(uint64 epoch, bytes32 closureHash) external onlyDelegateCall {
        if (msg.sender != ZONE_FACTORY_ADDRESS) revert NotFactory();
        FastEpochConfig storage config = _fastEpochs[epoch];
        if (epoch == 0 || epoch != fastEpoch || config.closed || config.retired || closureHash == bytes32(0)) {
            revert InvalidFastEpoch();
        }
        config.closed = true;
        config.closureHash = closureHash;
        emit FastEpochClosed(epoch, closureHash);
    }

    /// @inheritdoc IZonePortal
    function recordFastPeerBarrier(
        FastBarrierStatement calldata statement,
        bytes[] calldata signatures
    ) external onlyDelegateCall {
        if (msg.sender != ZONE_FACTORY_ADDRESS) revert NotFactory();
        FastEpochConfig storage config = _fastEpochs[statement.destinationEpoch];
        if (
            statement.destinationPortal != address(this) || statement.destinationEpoch == 0
                || statement.destinationEpoch != fastEpoch || !config.closed || config.retired
                || statement.closureHash != config.closureHash || statement.sourcePortal == address(this)
                || statement.sourceEpoch == 0 || statement.importedAnchorHash == bytes32(0)
                || statement.blockHash == bytes32(0) || statement.stateRoot == bytes32(0)
                || statement.completeLockRoot == bytes32(0)
                || (statement.unresolvedCount == 0 && statement.unresolvedRoot != FAST_EMPTY_UNRESOLVED_ROOT)
                || (statement.unresolvedCount != 0
                    && (statement.unresolvedRoot == bytes32(0)
                        || statement.unresolvedRoot == FAST_EMPTY_UNRESOLVED_ROOT))
                || !_isFastEpochPeer[statement.destinationEpoch][statement.sourcePortal]
        ) {
            revert InvalidFastEpoch();
        }
        if (_fastPeerBarriers[statement.destinationEpoch][statement.sourcePortal].recorded) {
            revert FastPeerBarrierAlreadyRecorded(statement.sourcePortal);
        }
        bytes32 barrierHash = keccak256(abi.encode(FAST_BARRIER_DOMAIN, block.chainid, statement));
        _verifyHistoricalFastQuorum(statement.sourcePortal, statement.sourceEpoch, barrierHash, signatures);
        _fastPeerBarriers[statement.destinationEpoch][statement.sourcePortal] = FastPeerBarrier({
            recorded: true,
            finalized: false,
            sourceEpoch: statement.sourceEpoch,
            importedAnchorNumber: statement.importedAnchorNumber,
            importedAnchorHash: statement.importedAnchorHash,
            logTerm: statement.logTerm,
            logIndex: statement.logIndex,
            blockHeight: statement.blockHeight,
            blockHash: statement.blockHash,
            stateRoot: statement.stateRoot,
            lockLogWatermark: statement.lockLogWatermark,
            completeLockRoot: statement.completeLockRoot,
            unresolvedRoot: statement.unresolvedRoot,
            unresolvedCount: statement.unresolvedCount,
            barrierHash: barrierHash,
            terminalRoot: bytes32(0),
            dispositionRoot: bytes32(0),
            resolvedCount: 0,
            remainingUnresolvedRoot: bytes32(0),
            remainingUnresolvedCount: 0,
            resolutionHash: bytes32(0)
        });
        config.recordedPeerBarriers += 1;
        emit FastPeerBarrierRecorded(
            statement.destinationEpoch,
            statement.sourcePortal,
            statement.sourceEpoch,
            statement.lockLogWatermark,
            barrierHash,
            statement.completeLockRoot,
            statement.unresolvedRoot,
            statement.unresolvedCount
        );
    }

    /// @inheritdoc IZonePortal
    function finalizeFastPeerBarrier(
        uint64 epoch,
        address peerPortal,
        FastBarrierResolution calldata resolution,
        bytes[] calldata signatures
    ) external onlyDelegateCall {
        if (msg.sender != ZONE_FACTORY_ADDRESS) revert NotFactory();
        FastEpochConfig storage config = _fastEpochs[epoch];
        FastPeerBarrier storage barrier = _fastPeerBarriers[epoch][peerPortal];
        if (
            !config.closed || config.retired || !barrier.recorded || barrier.finalized
                || resolution.barrierHash != barrier.barrierHash || resolution.terminalRoot == bytes32(0)
                || resolution.dispositionRoot == bytes32(0) || resolution.resolvedCount != barrier.unresolvedCount
                || resolution.remainingUnresolvedRoot != FAST_EMPTY_UNRESOLVED_ROOT
                || resolution.remainingUnresolvedCount != 0
        ) revert InvalidFastCertificate();
        bytes32 resolutionHash = keccak256(
            abi.encode(FAST_BARRIER_RESOLUTION_DOMAIN, block.chainid, address(this), epoch, peerPortal, resolution)
        );
        _verifyHistoricalFastQuorum(peerPortal, barrier.sourceEpoch, resolutionHash, signatures);
        barrier.finalized = true;
        barrier.terminalRoot = resolution.terminalRoot;
        barrier.dispositionRoot = resolution.dispositionRoot;
        barrier.resolvedCount = resolution.resolvedCount;
        barrier.remainingUnresolvedRoot = resolution.remainingUnresolvedRoot;
        barrier.remainingUnresolvedCount = resolution.remainingUnresolvedCount;
        barrier.resolutionHash = resolutionHash;
        config.finalizedPeerBarriers += 1;
        emit FastPeerBarrierFinalized(
            epoch, peerPortal, resolutionHash, resolution.terminalRoot, resolution.dispositionRoot
        );
    }

    /// @inheritdoc IZonePortal
    function recordFastFinalSettlement(
        uint64 epoch,
        uint256 finalZoneHeight,
        bytes32 finalBlockHash,
        uint64 finalWithdrawalBatchIndex,
        bytes[] calldata signatures
    ) external onlyDelegateCall {
        if (msg.sender != ZONE_FACTORY_ADDRESS) revert NotFactory();
        FastEpochConfig storage config = _fastEpochs[epoch];
        if (
            !config.closed || config.retired || config.recordedPeerBarriers != config.expectedPeerBarriers
                || config.finalizedPeerBarriers != config.expectedPeerBarriers
                || config.finalSettlementHash != bytes32(0) || zoneHeight != finalZoneHeight
                || blockHash != finalBlockHash || withdrawalBatchIndex != finalWithdrawalBatchIndex
        ) revert FastEpochNotDrained(epoch, config.finalizedPeerBarriers, config.expectedPeerBarriers);
        bytes32 barriersHash = FAST_BARRIERS_DOMAIN;
        address[] storage peers = _fastEpochPeers[epoch];
        for (uint256 i; i < peers.length; ++i) {
            FastPeerBarrier storage barrier = _fastPeerBarriers[epoch][peers[i]];
            if (!barrier.finalized || barrier.resolutionHash == bytes32(0)) {
                revert FastEpochNotDrained(epoch, config.finalizedPeerBarriers, config.expectedPeerBarriers);
            }
            barriersHash = keccak256(
                abi.encode(barriersHash, peers[i], barrier.barrierHash, barrier.resolutionHash)
            );
        }
        bytes32 settlementHash = keccak256(
            abi.encode(
                FAST_FINAL_SETTLEMENT_DOMAIN,
                block.chainid,
                address(this),
                epoch,
                config.rosterHash,
                config.closureHash,
                finalZoneHeight,
                finalBlockHash,
                finalWithdrawalBatchIndex,
                barriersHash
            )
        );
        _verifyHistoricalFastQuorum(address(this), epoch, settlementHash, signatures);
        config.finalSettlementHeight = finalZoneHeight;
        config.finalSettlementBlockHash = finalBlockHash;
        config.finalSettlementWithdrawalBatchIndex = finalWithdrawalBatchIndex;
        config.barriersHash = barriersHash;
        config.finalSettlementHash = settlementHash;
        emit FastFinalSettlementRecorded(
            epoch, finalZoneHeight, finalBlockHash, finalWithdrawalBatchIndex, settlementHash
        );
    }

    /// @inheritdoc IZonePortal
    function installFastCheckpoint(
        FastCheckpointStatement calldata statement,
        address[] calldata nextMembers,
        bytes[] calldata signatures
    ) external onlyDelegateCall {
        if (msg.sender != ZONE_FACTORY_ADDRESS) revert NotFactory();
        FastEpochConfig storage config = _fastEpochs[statement.oldEpoch];
        if (
            config.finalSettlementHash == bytes32(0) || config.checkpointHash != bytes32(0)
                || statement.portal != address(this) || statement.oldEpoch == 0
                || statement.nextEpoch <= statement.oldEpoch || statement.nextRosterHash == bytes32(0)
                || statement.finalZoneHeight != config.finalSettlementHeight
                || statement.finalBlockHash != config.finalSettlementBlockHash
                || statement.finalWithdrawalBatchIndex != config.finalSettlementWithdrawalBatchIndex
                || statement.finalSettlementHash != config.finalSettlementHash
                || statement.checkpointStateRoot == bytes32(0)
        ) revert InvalidFastCertificate();
        bytes32 checkpointHash = keccak256(abi.encode(FAST_CHECKPOINT_DOMAIN, block.chainid, statement));
        _verifyNextRosterQuorum(nextMembers, checkpointHash, signatures);
        config.nextEpoch = statement.nextEpoch;
        config.nextRosterHash = statement.nextRosterHash;
        config.checkpointLogTerm = statement.checkpointLogTerm;
        config.checkpointLogIndex = statement.checkpointLogIndex;
        config.checkpointHeight = statement.checkpointHeight;
        config.checkpointBlockHash = statement.checkpointBlockHash;
        config.checkpointStateRoot = statement.checkpointStateRoot;
        config.checkpointHash = checkpointHash;
        emit FastCheckpointInstalled(statement.oldEpoch, statement.nextEpoch, checkpointHash);
    }

    /// @inheritdoc IZonePortal
    function retireFastEpoch(uint64 epoch) external onlyDelegateCall {
        if (msg.sender != ZONE_FACTORY_ADDRESS) revert NotFactory();
        FastEpochConfig storage config = _fastEpochs[epoch];
        if (
            !config.closed || config.retired || config.recordedPeerBarriers != config.expectedPeerBarriers
                || config.finalizedPeerBarriers != config.expectedPeerBarriers
                || config.finalSettlementHash == bytes32(0) || config.checkpointHash == bytes32(0)
        ) revert FastEpochNotDrained(epoch, config.finalizedPeerBarriers, config.expectedPeerBarriers);
        config.retired = true;
        emit FastEpochRetired(epoch);
    }

    function _verifyHistoricalFastQuorum(
        address sourcePortal,
        uint64 sourceEpoch,
        bytes32 digest,
        bytes[] calldata signatures
    ) private view {
        if (sourceEpoch == 0 || signatures.length != 2) revert InvalidFastCertificate();
        address first;
        for (uint256 i; i < signatures.length; ++i) {
            address signer;
            try StdPrecompiles.SIGNATURE_VERIFIER.recover(digest, signatures[i]) returns (address recovered) {
                signer = recovered;
            } catch {
                revert InvalidFastCertificate();
            }
            (bool success, bytes memory result) = sourcePortal.staticcall(
                abi.encodeCall(IZonePortal.isFastEpochMember, (sourceEpoch, signer))
            );
            if (!success || result.length != 32) {
                revert InvalidFastCertificate();
            }
            bool authorized = abi.decode(result, (bool));
            if (signer == address(0) || signer == first || !authorized) {
                revert InvalidFastCertificate();
            }
            first = signer;
        }
    }

    function _verifierAcceptsInvalidProof() private view returns (bool) {
        try IVerifier(verifier).verify(
            zoneId,
            0,
            0,
            bytes32(0),
            0,
            0,
            BlockTransition({prevBlockHash: bytes32(0), nextBlockHash: bytes32(0)}),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0),
                nextProcessedHash: bytes32(0),
                prevDepositNumber: 0,
                nextDepositNumber: 0
            }),
            TokenEnablementTransition({prevProcessedTokenCount: 0, nextProcessedTokenCount: 0}),
            bytes32(0),
            "",
            ""
        ) returns (bool accepted) {
            return accepted;
        } catch {
            return false;
        }
    }

    function _verifyNextRosterQuorum(
        address[] calldata members,
        bytes32 digest,
        bytes[] calldata signatures
    ) private view {
        if (
            members.length != 3 || signatures.length != 2 || members[0] == address(0)
                || members[1] == address(0) || members[2] == address(0) || members[0] == members[1]
                || members[0] == members[2] || members[1] == members[2]
        ) revert InvalidFastCertificate();
        address first;
        for (uint256 i; i < signatures.length; ++i) {
            address signer;
            try StdPrecompiles.SIGNATURE_VERIFIER.recover(digest, signatures[i]) returns (address recovered) {
                signer = recovered;
            } catch {
                revert InvalidFastCertificate();
            }
            if (
                signer == address(0) || signer == first
                    || (signer != members[0] && signer != members[1] && signer != members[2])
            ) revert InvalidFastCertificate();
            first = signer;
        }
    }

    function _rejectLegacyAuthorityChangeDuringFastEpoch() private view {
        if (fastEpochActive()) revert FastEpochActive(fastEpoch);
    }

    /// @dev Single write path for a leadership transition: assign, bump the fencing epoch,
    ///      stamp the activation block, emit. `crates/l1` decodes `LeaderUpdated` to drive
    ///      node roles, so every transition must go through here to stay consistent.
    function _setLeader(address newLeader) private {
        address previous = leader;
        leader = newLeader;
        leaderEpoch += 1;
        uint64 activationTempoBlock = uint64(block.number);
        leaderActivationTempoBlock = activationTempoBlock;
        emit LeaderUpdated(previous, newLeader, leaderEpoch, activationTempoBlock);
    }

    /// @notice Set zone gas rate. Only callable by admin.
    /// @dev The admin publishes the operational rate and receives collected deposit fees.
    /// @param _zoneGasRate Zone token units per gas unit on the zone
    function setZoneGasRate(uint128 _zoneGasRate) external onlyAdmin {
        if (_zoneGasRate > MAX_GAS_FEE_RATE) revert GasFeeRateTooHigh();
        zoneGasRate = _zoneGasRate;
        emit ZoneGasRateUpdated(_zoneGasRate);
    }

    /// @notice Set the maximum Tempo gas rate a sequencer may configure on the zone-side outbox.
    function setMaxTempoGasRate(uint128 _maxTempoGasRate) external onlyAdmin {
        if (_maxTempoGasRate > MAX_GAS_FEE_RATE) revert GasFeeRateTooHigh();
        maxTempoGasRate = _maxTempoGasRate;
        emit MaxTempoGasRateUpdated(_maxTempoGasRate);
    }

    /// @notice Set the gas amount used to price failed-deposit bounce-backs on Tempo.
    /// @dev Only the admin can change the amount because it determines the fee deducted from a
    ///      failed deposit at processing time.
    function setBouncebackGas(uint64 _bouncebackGas) external onlyAdmin {
        bouncebackGas = _bouncebackGas;
        emit BouncebackGasUpdated(_bouncebackGas);
    }

    /*//////////////////////////////////////////////////////////////
                             ADMIN MANAGEMENT
    //////////////////////////////////////////////////////////////*/

    /// @notice Start an admin transfer. Only callable by the current admin.
    /// @dev Two-step handoff: the new admin only takes over once it calls
    ///      {acceptAdmin}, which prevents fat-fingered transfers.
    ///      Passing address(0) cancels a pending transfer.
    /// @param newAdmin The address that will become admin after accepting (address(0) cancels).
    function transferAdmin(address newAdmin) external onlyAdmin {
        pendingAdmin = newAdmin;
        emit AdminTransferStarted(admin, newAdmin);
    }

    /// @notice Accept a pending admin transfer. Only callable by the pending admin.
    /// @dev The explicit `pendingAdmin == address(0)` check because it is technically
    ///      possible to make a system tx on L1 with msg.sender == 0.
    ///      The Admin key can only be rotated, never renounced.
    function acceptAdmin() external {
        if (pendingAdmin == address(0) || msg.sender != pendingAdmin) revert NotPendingAdmin();
        address previousAdmin = admin;
        admin = pendingAdmin;
        pendingAdmin = address(0);
        emit AdminTransferred(previousAdmin, admin);
    }

    /// @notice Enable or disable account allowlist enforcement without discarding membership.
    function setAccessMode(bool enforced) external onlyAdmin {
        _requireCapabilityActive(Capability.AccessPolicy);
        _isAccessEnforced = enforced;
        emit EnforcementModesUpdated(enforced, _isGatewayEnforced);
    }

    /// @notice Enable or disable callback gateway registration enforcement.
    function setGatewayMode(bool enforced) external onlyAdmin {
        _requireCapabilityActive(Capability.AccessPolicy);
        _isGatewayEnforced = enforced;
        emit EnforcementModesUpdated(_isAccessEnforced, enforced);
    }

    /// @notice Return whether account allowlist enforcement is enabled.
    function isAccessEnforced() public view returns (bool) {
        return _isAccessEnforced;
    }

    /// @notice Return whether callback gateway registration enforcement is disabled.
    function isGatewayOpen() public view returns (bool) {
        return !_isGatewayEnforced;
    }

    /// @notice Add or remove an account from closed-loop portal flows.
    /// @dev Returns without emitting when already configured. Abdication freezes all changes.
    function setAllowedAccount(address account, bool allowed) external onlyAdmin {
        _requireCapabilityActive(Capability.AccessPolicy);
        if (allowed) require(account != messenger);
        Role previous = role[account];
        Role next = allowed ? Role.Account : Role.None;
        if (previous == next) return;
        require(previous == (allowed ? Role.None : Role.Account));
        role[account] = next;
        emit RoleUpdated(account, previous, next);
    }

    /// @notice Add or remove a callback gateway.
    /// @dev Returns without emitting when already configured. Abdication freezes all changes.
    function setGateway(address account, bool allowed) external onlyAdmin {
        _requireCapabilityActive(Capability.AccessPolicy);
        Role previous = role[account];
        Role next = allowed ? Role.CallbackGateway : Role.None;
        if (previous == next) return;
        require(previous == (allowed ? Role.None : Role.CallbackGateway));
        role[account] = next;
        emit RoleUpdated(account, previous, next);
    }

    /// @notice Add or remove a pause guardian.
    /// @dev Pause-capability abdication freezes both additions and removals.
    function setPauseGuardian(address account, bool allowed) external onlyAdmin {
        _requireCapabilityActive(Capability.PausePortal);
        Role previous = role[account];
        Role next = allowed ? Role.PauseGuardian : Role.None;
        if (previous == next) return;
        require(previous == (allowed ? Role.None : Role.PauseGuardian));
        role[account] = next;
        emit RoleUpdated(account, previous, next);
    }

    function hasRole(address account, Role expected) public view returns (bool) {
        return role[account] == expected;
    }

    /*//////////////////////////////////////////////////////////////
                           QUEUE ACCESSORS
    //////////////////////////////////////////////////////////////*/

    function withdrawalQueueHead() external view returns (uint256) {
        return _withdrawalQueue.head;
    }

    function withdrawalQueueTail() external view returns (uint256) {
        return _withdrawalQueue.tail;
    }

    function withdrawalQueueSlot(uint256 queueIndex) external view returns (bytes32) {
        return _withdrawalQueue.slots[queueIndex];
    }

    /*//////////////////////////////////////////////////////////////
                          TOKEN REGISTRY
    //////////////////////////////////////////////////////////////*/

    /// @notice Check if a token is enabled for bridging
    function isTokenEnabled(address _token) external view returns (bool) {
        return _tokenConfigs[_token].enabled;
    }

    /// @notice Check if deposits are currently active for a token
    function areDepositsActive(address _token) external view returns (bool) {
        TokenConfig storage cfg = _tokenConfigs[_token];
        return !paused() && cfg.enabled && cfg.depositsActive;
    }

    /// @notice Get the token configuration for a specific token
    function tokenConfig(address _token) external view returns (TokenConfig memory) {
        return _tokenConfigs[_token];
    }

    /// @notice Get the number of enabled tokens
    function enabledTokenCount() external view returns (uint256) {
        return _enabledTokens.length;
    }

    /// @notice Get an enabled token by index
    function enabledTokenAt(uint256 index) external view returns (address) {
        return _enabledTokens[index];
    }

    /// @notice Whether deposits and withdrawal processing are currently paused.
    /// @dev The pause expires automatically once block.timestamp reaches pauseExpiry.
    function paused() public view returns (bool) {
        return block.timestamp < pauseExpiry;
    }

    /// @notice Pause deposits and withdrawal processing for 30 days.
    function pause() external whenNotPaused {
        _requireCapabilityActive(Capability.PausePortal);
        if (msg.sender != admin && !isSequencer(msg.sender) && !hasRole(msg.sender, Role.PauseGuardian)) {
            revert NotPauseAuthority();
        }
        pauseExpiry = uint64(block.timestamp) + PAUSE_DURATION;
        emit PortalPaused(msg.sender);
    }

    /// @notice Resume deposits and withdrawal processing before the pause expires.
    /// @dev Admin recovery remains available after the pause capability is abdicated.
    function resume() external onlyAdmin {
        pauseExpiry = 0;
        emit PortalResumed(msg.sender);
    }

    /// @notice Schedule permanent abdication of a Portal configuration surface.
    function abdicate(Capability capability) external onlyAdmin whenNotPaused {
        if (abdicationEffectiveAt[capability] != 0) revert AbdicationAlreadyScheduled(capability);
        uint64 effectiveAt = uint64(block.timestamp) + ABDICATION_DELAY;
        abdicationEffectiveAt[capability] = effectiveAt;
        emit AbdicationScheduled(capability, effectiveAt);
    }

    function _requireCapabilityActive(Capability capability) internal view {
        uint64 effectiveAt = abdicationEffectiveAt[capability];
        if (effectiveAt != 0 && block.timestamp >= effectiveAt) {
            revert CapabilityAbdicated(capability);
        }
    }

    /// @notice Enable a new TIP-20 token for bridging. Only callable by admin.
    /// @dev Irreversible: once enabled, a token cannot be disabled. Frozen until the first Zone
    ///      batch authenticates the enabled-token cursor.
    function enableToken(address _token) external onlyAdmin {
        if (_tokenConfigs[_token].enabled) revert TokenAlreadyEnabled();
        if (!ITIP20Factory(StdPrecompiles.TIP20_FACTORY_ADDRESS).isTIP20(_token)) {
            revert TokenNotEnabled();
        }
        if (!tokenEnablementCursorInitialized) revert TokenEnablementCursorNotInitialized();
        if (_enabledTokens.length - lastProcessedEnabledTokenCount >= MAX_UNPROCESSED_TOKEN_ENABLEMENTS) {
            revert TokenEnablementBlockCapacityExceeded(MAX_UNPROCESSED_TOKEN_ENABLEMENTS);
        }
        _enableTokenInternal(_token);
    }

    /// @notice Pause deposits for a token. Only callable by admin.
    /// @dev Does not affect withdrawal processing (non-custodial guarantee).
    function pauseDeposits(address _token) external onlyAdmin {
        if (!_tokenConfigs[_token].enabled) revert TokenNotEnabled();
        _tokenConfigs[_token].depositsActive = false;
        emit DepositsPaused(_token);
    }

    /// @notice Resume deposits for a token. Only callable by admin.
    function resumeDeposits(address _token) external onlyAdmin {
        if (!_tokenConfigs[_token].enabled) revert TokenNotEnabled();
        _tokenConfigs[_token].depositsActive = true;
        emit DepositsResumed(_token);
    }

    /// @notice Internal function to enable a token (used by initializer and enableToken)
    function _enableTokenInternal(address _token) internal {
        // Bound the metadata copied into the zone before mutating portal or policy state. The zone
        // must initialize every token emitted in this block inside advanceTempo's fixed gas budget.
        string memory name = ITIP20(_token).name();
        string memory symbol = ITIP20(_token).symbol();
        string memory currency = ITIP20(_token).currency();
        if (
            bytes(name).length > MAX_TOKEN_METADATA_BYTES || bytes(symbol).length > MAX_TOKEN_METADATA_BYTES
                || bytes(currency).length > MAX_TOKEN_METADATA_BYTES
        ) {
            revert TokenMetadataTooLong();
        }

        address[] memory tokens = new address[](1);
        tokens[0] = _token;

        (bool isSet,) = TIP403_REGISTRY.tokenTransferPolicyId(_token);
        if (!isSet) {
            TIP403_REGISTRY.migrateTransferPolicyIds(tokens);
            (isSet,) = TIP403_REGISTRY.tokenTransferPolicyId(_token);
        }
        if (!isSet) {
            revert TokenTransferPolicyNotSet();
        }

        tokenEnablementHash = keccak256(abi.encode(tokenEnablementHash, _token, name, symbol, currency));
        _tokenConfigs[_token] = TokenConfig({enabled: true, depositsActive: true});
        _enabledTokens.push(_token);

        emit TokenEnabled(_token, name, symbol, currency);
    }

    /// @notice Update the zone's operator RPC endpoint.
    /// @param _rpcUrl The new RPC url
    function setRpcUrl(string calldata _rpcUrl) external onlySequencer {
        rpcUrl = _rpcUrl;
        emit RpcUrlUpdated(_rpcUrl);
    }

    /*//////////////////////////////////////////////////////////////
                        ENCRYPTION KEY MANAGEMENT
    //////////////////////////////////////////////////////////////*/

    /// @notice Get the sequencer's current encryption public key
    /// @return x The X coordinate
    /// @return yParity The Y coordinate parity (0x02 or 0x03)
    /// @return pubkey The address derived from the public key
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity, address pubkey) {
        if (_encryptionKeys.length == 0) revert NoEncryptionKeySet();
        EncryptionKeyEntry storage current = _encryptionKeys[_encryptionKeys.length - 1];
        return (current.x, current.yParity, Secp256k1Lib.deriveAddress(current.x, current.yParity));
    }

    /// @notice Set the sequencer's encryption public key with proof of possession from its private key
    /// @dev Only callable by an active sequencer or the admin. Appends to key history.
    ///      No reentrancy guard is needed because this function makes no unrestricted external
    ///      calls; its only external calls are to fixed cryptographic precompiles.
    ///      Requires a valid ECDSA signature over keccak256(abi.encode(address(this), x, yParity))
    ///      produced by the private key corresponding to (x, yParity). This prevents accidental
    ///      registration of keys the sequencer cannot decrypt with.
    /// @param x The X coordinate (must be valid secp256k1 point)
    /// @param yParity The Y coordinate parity (0x02 or 0x03)
    /// @param popV Recovery id of the proof-of-possession signature
    /// @param popR R component of the proof-of-possession signature
    /// @param popS S component of the proof-of-possession signature
    function setSequencerEncryptionKey(bytes32 x, uint8 yParity, uint8 popV, bytes32 popR, bytes32 popS)
        external
        onlySequencerOrAdmin
    {
        // Validate yParity
        if (!Secp256k1Lib.isCompressedYParity(yParity)) revert InvalidEphemeralPubkey();

        // Validate x is on the secp256k1 curve
        if (!Secp256k1Lib.isValidX(x)) revert InvalidEphemeralPubkey();

        // Verify proof of possession: the caller must prove control of the encryption private key.
        bytes32 message = keccak256(abi.encode(address(this), x, yParity));
        address recovered = ecrecover(message, popV, popR, popS);
        address expected = Secp256k1Lib.deriveAddress(x, yParity);
        if (recovered == address(0) || recovered != expected) {
            revert InvalidProofOfPossession();
        }

        uint64 activationBlock = uint64(block.number);
        _encryptionKeys.push(EncryptionKeyEntry({x: x, yParity: yParity, activationBlock: activationBlock}));
        emit SequencerEncryptionKeyUpdated(x, yParity, expected, _encryptionKeys.length - 1, activationBlock);
    }

    /// @notice Get the number of keys in the history
    function encryptionKeyCount() external view returns (uint256) {
        return _encryptionKeys.length;
    }

    /// @notice Get a historical encryption key by index
    /// @param index The index in the key history (0 = first key)
    /// @return entry The key entry with activation block
    function encryptionKeyAt(uint256 index) external view returns (EncryptionKeyEntry memory entry) {
        if (index >= _encryptionKeys.length) {
            revert InvalidEncryptionKeyIndex(index);
        }
        return _encryptionKeys[index];
    }

    /// @notice Get the encryption key that was active at a specific Tempo block
    /// @dev Binary search through key history to find the correct key
    /// @param tempoBlockNumber The Tempo block number to query
    /// @return x The X coordinate of the active key
    /// @return yParity The Y coordinate parity
    /// @return keyIndex The index of this key in history
    function encryptionKeyAtBlock(uint64 tempoBlockNumber)
        external
        view
        returns (bytes32 x, uint8 yParity, uint256 keyIndex)
    {
        uint256 len = _encryptionKeys.length;
        if (len == 0 || tempoBlockNumber < _encryptionKeys[0].activationBlock) {
            revert NoEncryptionKeyAtBlock(tempoBlockNumber);
        }

        uint256 low = 0;
        uint256 high = len - 1;
        while (low < high) {
            uint256 mid = (low + high + 1) / 2;
            if (_encryptionKeys[mid].activationBlock <= tempoBlockNumber) {
                low = mid;
            } else {
                high = mid - 1;
            }
        }

        EncryptionKeyEntry storage entry = _encryptionKeys[low];
        return (entry.x, entry.yParity, low);
    }

    /// @notice Check if an encryption key is still valid for new deposits
    /// @param keyIndex The key index to check
    /// @return valid True if the key can be used for new deposits
    /// @return expiresAtBlock Block number when this key expires (0 if current key)
    function isEncryptionKeyValid(uint256 keyIndex) public view returns (bool valid, uint64 expiresAtBlock) {
        if (keyIndex >= _encryptionKeys.length) {
            return (false, 0);
        }

        // Current key (latest) never expires
        if (keyIndex == _encryptionKeys.length - 1) {
            return (true, 0);
        }

        // Old keys are valid during grace period after supersession
        EncryptionKeyEntry storage nextKey = _encryptionKeys[keyIndex + 1];
        uint64 expiration = nextKey.activationBlock + ENCRYPTION_KEY_GRACE_PERIOD;

        valid = block.number < expiration;
        expiresAtBlock = expiration;
    }

    /*//////////////////////////////////////////////////////////////
                               DEPOSITS
    //////////////////////////////////////////////////////////////*/

    /// @notice Calculate the fee for a deposit
    /// @dev Fee = FIXED_DEPOSIT_GAS * zoneGasRate
    /// @return fee The deposit fee in zone token units
    function calculateDepositFee() public view returns (uint128 fee) {
        fee = uint128(FIXED_DEPOSIT_GAS) * zoneGasRate;
    }

    /// @notice Calculate the reserved fee for a failed-deposit bounce-back
    /// @dev Fee = ceil(bouncebackGas * block.basefee / 1e12)
    /// @return fee The bounce-back fee in token units
    function calculateBouncebackFee() public view returns (uint128 fee) {
        uint256 gasFee = uint256(bouncebackGas) * block.basefee;
        // Round up after scaling so bounce-backs do not underpay.
        fee = uint128((gasFee + TEMPO_BASE_FEE_SCALE - 1) / TEMPO_BASE_FEE_SCALE);
    }

    function _validateDepositsActive(address _token) internal view {
        TokenConfig storage cfg = _tokenConfigs[_token];
        if (!cfg.enabled) revert TokenNotEnabled();
        if (!cfg.depositsActive) revert DepositsNotActive();
    }

    function _requireAllowed(address account) internal view {
        if (!_isAllowed(account)) revert AccountNotAllowed(account);
    }

    function _requireAllowedDepositor(address account) internal view {
        if (!_isAccessEnforced) return;
        if (_isGatewayEnforced && hasRole(account, Role.CallbackGateway)) {
            return;
        }
        if (!hasRole(account, Role.Account)) revert AccountNotAllowed(account);
    }

    function _isAllowed(address account) internal view returns (bool) {
        return !_isAccessEnforced || hasRole(account, Role.Account);
    }

    function _collectDepositFunds(address _token, uint128 amount) internal returns (uint128 fee, uint128 netAmount) {
        fee = calculateDepositFee();
        uint128 bouncebackFee = calculateBouncebackFee();
        if (amount < fee + bouncebackFee) revert DepositTooSmall();
        netAmount = amount - fee;

        // TIP-20 transfers revert on failure, so no boolean check is needed here.
        ITIP20(_token).transferFrom(msg.sender, address(this), amount);
        if (fee > 0) {
            ITIP20(_token).transfer(admin, fee);
        }
    }

    function _recordDeposit(bytes32 newCurrentDepositQueueHash, uint64 maximum) internal returns (uint64 thisDeposit) {
        if (depositCount - lastProcessedDepositNumber >= maximum) {
            revert DepositBlockCapacityExceeded(maximum);
        }

        currentDepositQueueHash = newCurrentDepositQueueHash;
        thisDeposit = ++depositCount;
    }

    /// @notice Alias for `depositEncrypted`.
    function deposit(
        address _token,
        uint128 amount,
        uint256 keyIndex,
        DepositPayload calldata encrypted,
        address tempoRefundRecipient
    ) external whenNotPaused returns (bytes32 newCurrentDepositQueueHash) {
        return _deposit(_token, amount, keyIndex, encrypted, tempoRefundRecipient);
    }

    /// @notice Deposit with encrypted recipient and memo
    /// @dev The encrypted payload contains (to, memo) encrypted to the sequencer's key.
    ///      The token identity is public (not encrypted) since the portal must escrow it.
    ///      Validates that keyIndex is valid (exists and not expired).
    ///      Charges the configured zone deposit fee.
    /// @param _token The TIP-20 token to deposit
    /// @param amount Amount to deposit (fee deducted from this amount)
    /// @param keyIndex Index of the encryption key used (from encryptionKeyAt)
    /// @param encrypted The encrypted payload (recipient and memo)
    /// @return newCurrentDepositQueueHash The new deposit queue hash
    function depositEncrypted(
        address _token,
        uint128 amount,
        uint256 keyIndex,
        DepositPayload calldata encrypted,
        address tempoRefundRecipient
    ) public whenNotPaused returns (bytes32 newCurrentDepositQueueHash) {
        return _deposit(_token, amount, keyIndex, encrypted, tempoRefundRecipient);
    }

    function _deposit(
        address _token,
        uint128 amount,
        uint256 keyIndex,
        DepositPayload calldata encrypted,
        address tempoRefundRecipient
    ) internal returns (bytes32 newCurrentDepositQueueHash) {
        if (tempoRefundRecipient == address(0)) revert InvalidBouncebackRecipient();
        // Enforced gateways may deposit callback returns without also being allowed accounts.
        _requireAllowedDepositor(msg.sender);
        _requireAllowed(tempoRefundRecipient);

        _validateDepositsActive(_token);

        uint64 policyId = ITIP20(_token).transferPolicyId();
        if (!TIP403_REGISTRY.isAuthorizedRecipient(policyId, tempoRefundRecipient)) {
            revert ITIP20.PolicyForbids();
        }

        // Validate ephemeral public key is a valid secp256k1 point
        // Prevents griefing: invalid points make Chaum-Pedersen proofs impossible,
        // which would block chain progress on the zone side.
        if (!Secp256k1Lib.isCompressedYParity(encrypted.ephemeralPubkeyYParity)) {
            revert InvalidEphemeralPubkey();
        }
        if (!Secp256k1Lib.isValidX(encrypted.ephemeralPubkeyX)) {
            revert InvalidEphemeralPubkey();
        }

        // Validate ciphertext length — GCM ciphertext == plaintext length (tag is separate)
        // Prevents DoS: oversized ciphertexts inflate zone-side AES-GCM processing cost
        if (encrypted.ciphertext.length != ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE) {
            revert InvalidCiphertextLength(encrypted.ciphertext.length, ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE);
        }

        // Validate encryption key
        (bool valid,) = isEncryptionKeyValid(keyIndex);
        if (!valid) {
            if (keyIndex >= _encryptionKeys.length) {
                revert InvalidEncryptionKeyIndex(keyIndex);
            }
            EncryptionKeyEntry storage key = _encryptionKeys[keyIndex];
            EncryptionKeyEntry storage nextKey = _encryptionKeys[keyIndex + 1];
            revert EncryptionKeyExpired(keyIndex, key.activationBlock, nextKey.activationBlock);
        }

        (uint128 fee, uint128 netAmount) = _collectDepositFunds(_token, amount);

        // Build the queued deposit.
        Deposit memory depositData = Deposit({
            token: _token,
            sender: msg.sender,
            amount: netAmount,
            tempoRefundRecipient: tempoRefundRecipient,
            keyIndex: keyIndex,
            encrypted: encrypted
        });

        // Insert the deposit into the queue.
        newCurrentDepositQueueHash = DepositQueueLib.enqueueDeposit(currentDepositQueueHash, depositData);
        uint64 maximum = MAX_UNPROCESSED_DEPOSITS - WITHDRAWAL_PROCESSING_DEPOSIT_RESERVE;

        // A withdrawal callback may return one deposit through the capacity reserved from
        // public deposits. Further deposits remain subject to the public cap.
        if (_withdrawalReentrancyStatus == CALLBACK_DEPOSIT_AVAILABLE) {
            _withdrawalReentrancyStatus = CALLBACK_DEPOSIT_CONSUMED;
            maximum = MAX_UNPROCESSED_DEPOSITS;
        }

        uint64 thisDeposit = _recordDeposit(newCurrentDepositQueueHash, maximum);

        emit DepositMade(
            newCurrentDepositQueueHash,
            msg.sender,
            _token,
            netAmount,
            fee,
            keyIndex,
            encrypted.ephemeralPubkeyX,
            encrypted.ephemeralPubkeyYParity,
            encrypted.ciphertext,
            encrypted.nonce,
            encrypted.tag,
            tempoRefundRecipient,
            thisDeposit
        );
    }

    /*//////////////////////////////////////////////////////////////
                             WITHDRAWALS
    //////////////////////////////////////////////////////////////*/

    /// @notice Process multiple withdrawals from the queue in a single transaction.
    /// @dev Withdrawals must be supplied in queue order. `remainingQueue` is the queue suffix
    ///      after the last supplied withdrawal, or zero if the batch exhausts the current slot.
    ///      Plain-transfer and callback failures bounce back without blocking the FIFO.
    function processWithdrawals(Withdrawal[] calldata withdrawals, bytes32 remainingQueue)
        external
        whenNotPaused
        nonReentrantWithdrawal
    {
        uint64 currentFastEpoch = fastEpoch;
        if (fastEpochActive()) {
            if (!_isFastEpochMember[currentFastEpoch][msg.sender]) revert NotSequencer();
            if (_fastEpochs[currentFastEpoch].finalSettlementHash != bytes32(0)) {
                revert InvalidFastEpoch();
            }
        } else if (!isSequencer(msg.sender)) {
            revert NotSequencer();
        }
        uint256 unprocessed = depositCount - lastProcessedDepositNumber;
        if (unprocessed > MAX_UNPROCESSED_DEPOSITS || withdrawals.length > MAX_UNPROCESSED_DEPOSITS - unprocessed) {
            revert DepositBlockCapacityExceeded(MAX_UNPROCESSED_DEPOSITS);
        }
        bytes32[] memory remainingQueues = new bytes32[](withdrawals.length);
        bytes32 nextQueue = remainingQueue;

        for (uint256 i = withdrawals.length; i > 0; --i) {
            remainingQueues[i - 1] = nextQueue;
            nextQueue = keccak256(abi.encode(withdrawals[i - 1], nextQueue));
        }

        for (uint256 i; i < withdrawals.length; ++i) {
            _processWithdrawal(withdrawals[i], remainingQueues[i]);
        }
    }

    function _processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) internal {
        // Pop from withdrawal queue (library handles swap and hash verification)
        _withdrawalQueue.dequeue(withdrawal, remainingQueue);

        address _token = withdrawal.token;

        if (withdrawal.fallbackNonce == 0) {
            _processDepositBounceBack(withdrawal);
            return;
        }

        if (withdrawal.gasLimit > MAX_WITHDRAWAL_GAS_LIMIT) {
            _enqueueBounceBack(_token, withdrawal.amount, withdrawal.fallbackNonce);
            emit WithdrawalProcessed(withdrawal.to, withdrawal.senderTag, _token, withdrawal.amount, false);
            return;
        }

        bool success;
        if (withdrawal.gasLimit == 0) {
            // Re-check current roles without reverting so an in-flight withdrawal to a revoked
            // account or newly registered gateway bounces without blocking the FIFO.
            success = (!_isGatewayEnforced || !hasRole(withdrawal.to, Role.CallbackGateway))
                && _isAllowed(withdrawal.to) && _tryTransfer(_token, withdrawal.to, withdrawal.amount);
        } else {
            // Isolate callback effects so failure can be caught without reverting the dequeue.
            try this.deliverWithdrawal(
                _token,
                withdrawal.to,
                withdrawal.amount,
                withdrawal.senderTag,
                withdrawal.gasLimit,
                withdrawal.callbackData
            ) {
                success = true;
            } catch {
                success = false;
            }
        }

        if (!success) {
            _enqueueBounceBack(_token, withdrawal.amount, withdrawal.fallbackNonce);
        }
        emit WithdrawalProcessed(withdrawal.to, withdrawal.senderTag, _token, withdrawal.amount, success);
    }

    /// @notice Deliver a callback withdrawal in a revertable self-call frame.
    /// @dev Only callable by this portal. processWithdrawals catches failures and bounces back.
    function deliverWithdrawal(
        address token,
        address target,
        uint128 amount,
        bytes32 senderTag,
        uint64 gasLimit,
        bytes calldata data
    ) external onlySelf {
        if (_isGatewayEnforced && !hasRole(target, Role.CallbackGateway)) {
            revert InvalidCallbackTarget();
        }
        if (!ITIP20(token).transfer(messenger, amount)) {
            revert TransferFailed();
        }

        bytes32 depositQueueHashBefore = currentDepositQueueHash;

        _withdrawalReentrancyStatus = CALLBACK_DEPOSIT_AVAILABLE;

        // We copy whatever the messenger reverts with, so keep its errors small.
        IZoneMessenger(messenger).relayMessage(zoneId, token, senderTag, target, amount, gasLimit, data);

        // Return to the normal withdrawal-processing state. If relayMessage reverts, this write
        // and any callback deposit are reverted together.
        _withdrawalReentrancyStatus = WITHDRAWAL_PROCESSING;

        // In closed access, this proves only that some deposit was appended to this portal; it does
        // not bind that deposit to the callback's token, amount, or recipient. Callback data is
        // opaque, so an enforced gateway is trusted to constrain the operation and return the
        // intended result. Open access imposes no source-deposit invariant: callback value may go
        // to another zone or leave the zone system entirely.
        if (_isAccessEnforced && currentDepositQueueHash == depositQueueHashBefore) {
            revert CallbackDidNotReturnToZone();
        }
    }

    function _processDepositBounceBack(Withdrawal calldata withdrawal) internal {
        address _token = withdrawal.token;
        uint128 bouncebackFee = calculateBouncebackFee();
        if (bouncebackFee > withdrawal.amount) {
            bouncebackFee = withdrawal.amount;
        }
        // Only deduct the fee if the admin transfer succeeds; otherwise the full amount remains
        // refundable to the deposit recipient.
        uint128 collectedFee;
        if (bouncebackFee > 0 && _tryTransfer(_token, admin, bouncebackFee)) {
            collectedFee = bouncebackFee;
        }
        uint128 refundAmount = withdrawal.amount - collectedFee;

        bool success = _isAllowed(withdrawal.to) && _tryTransfer(_token, withdrawal.to, refundAmount);

        if (success) {
            emit DepositBounceBack(withdrawal.to, _token, refundAmount, collectedFee);
        } else {
            refunds[_token][withdrawal.to] += refundAmount;
            emit DepositBounceBackPending(withdrawal.to, _token, refundAmount, collectedFee);
        }
    }

    function claimRefund(address token) external returns (uint128 amount) {
        _requireAllowed(msg.sender);
        amount = refunds[token][msg.sender];
        refunds[token][msg.sender] = 0;

        if (!_tryTransfer(token, msg.sender, amount)) revert CallbackRejected();

        emit RefundClaimed(msg.sender, token, amount);
    }

    /// @notice Attempt a TIP-20 transfer without bubbling recipient/policy reverts.
    /// @dev Returns false if the receive policy blocks direct delivery, or if the token transfer
    ///      reverts or returns false. Callers decide whether a failed transfer should be ignored,
    ///      parked for refund, or reverted.
    /// @param token The TIP-20 token to transfer.
    /// @param to The recipient address.
    /// @param amount The token amount to transfer.
    /// @return success True if the transfer completed directly to `to` and returned true.
    function _tryTransfer(address token, address to, uint128 amount) internal returns (bool success) {
        address effectiveRecipient;
        try StdPrecompiles.ADDRESS_REGISTRY.resolveRecipient(to) returns (address resolved) {
            effectiveRecipient = resolved;
        } catch {
            return false;
        }

        try TIP403_REGISTRY.validateReceivePolicy(token, address(this), effectiveRecipient) returns (
            bool authorized, ITIP403Registry.BlockedReason
        ) {
            if (!authorized) return false;
        } catch {
            return false;
        }

        try ITIP20(token).transfer(to, amount) returns (bool ok) {
            return ok;
        } catch {
            return false;
        }
    }

    /// @notice Enqueue a bounce-back deposit for failed callback
    /// @param _token The token from the failed withdrawal
    /// @param amount The amount to bounce back
    /// @param fallbackNonce The nonce resolving to the zone bounce-back recipient
    function _enqueueBounceBack(address _token, uint128 amount, uint64 fallbackNonce) internal {
        WithdrawalBounceBackDeposit memory depositData =
            WithdrawalBounceBackDeposit({token: _token, to: address(uint160(fallbackNonce)), amount: amount});

        bytes32 newCurrentDepositQueueHash = DepositQueueLib.enqueue(currentDepositQueueHash, depositData);
        uint64 thisDeposit = _recordDeposit(newCurrentDepositQueueHash, MAX_UNPROCESSED_DEPOSITS);

        emit WithdrawalBounceBack(newCurrentDepositQueueHash, fallbackNonce, _token, amount, thisDeposit);
    }

    /*//////////////////////////////////////////////////////////////
                           BATCH SUBMISSION
    //////////////////////////////////////////////////////////////*/

    /// @inheritdoc IZonePortal
    function submitBatch(
        uint64 tempoBlockNumber,
        uint64 recentTempoBlockNumber,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        TokenEnablementTransition calldata tokenEnablementTransition,
        bytes32 withdrawalQueueHash,
        bytes calldata verifierConfig,
        bytes calldata proof,
        uint256 nextZoneHeight,
        bytes[] calldata signatures
    ) external {
        uint64 currentFastEpoch = fastEpoch;
        bool useFastAuthority = fastEpochActive();
        if (useFastAuthority) {
            if (!_isFastEpochMember[currentFastEpoch][msg.sender]) revert NotSequencer();
            // Once the exact accepted prefix is recorded as final, neither settlement nor
            // withdrawal queue state may advance while checkpoint installation/retirement runs.
            if (_fastEpochs[currentFastEpoch].finalSettlementHash != bytes32(0)) {
                revert InvalidFastEpoch();
            }
            FastEpochConfig storage fastConfig = _fastEpochs[currentFastEpoch];
            if (
                fastConfig.proofMode == FastProofMode.Unset
                    || verifier.codehash != fastConfig.expectedVerifierCodeHash
                    || keccak256(verifierConfig) != fastConfig.expectedVerifierConfigHash
                    || (fastConfig.proofMode == FastProofMode.ProofRequired
                        && (proof.length == 0
                            || fastConfig.expectedVerifierCodeHash == T13_PROTOTYPE_VERIFIER_CODE_HASH
                            || fastConfig.expectedVerifierCodeHash
                                == DEVELOPMENT_PROTOTYPE_VERIFIER_CODE_HASH))
            ) revert InvalidFastProofConfiguration();
        } else if (!isSequencer(msg.sender)) {
            revert NotSequencer();
        }
        if (blockTransition.prevBlockHash != blockHash) {
            revert InvalidProof();
        }

        // Determine anchor block: either tempoBlockNumber (direct) or recentTempoBlockNumber (ancestry)
        uint64 anchorBlockNumber;
        bytes32 anchorBlockHash;

        if (recentTempoBlockNumber == 0) {
            // Direct mode: read tempoBlockNumber hash from EIP-2935
            anchorBlockNumber = tempoBlockNumber;
            if (tempoBlockNumber > block.number) {
                revert InvalidTempoBlockNumber();
            }

            anchorBlockHash = getBlockHash(tempoBlockNumber);
        } else {
            // Ancestry mode: read recentTempoBlockNumber hash, proof verifies ancestry chain
            if (recentTempoBlockNumber <= tempoBlockNumber) {
                revert InvalidTempoBlockNumber();
            }
            if (recentTempoBlockNumber > block.number) {
                revert InvalidTempoBlockNumber();
            }

            anchorBlockNumber = recentTempoBlockNumber;
            anchorBlockHash = getBlockHash(recentTempoBlockNumber);
        }

        if (anchorBlockHash == bytes32(0)) revert InvalidTempoBlockNumber();

        // The certificate binds every value that affects settlement, rather than only the
        // zone block hash. A leader therefore cannot reuse signatures for this block with a
        // different withdrawal root, deposit transition, Tempo anchor, or verifier config.
        if (!_verifySettlement(
                nextZoneHeight,
                tempoBlockNumber,
                anchorBlockNumber,
                anchorBlockHash,
                blockTransition,
                depositQueueTransition,
                tokenEnablementTransition,
                withdrawalQueueHash,
                verifierConfig,
                signatures
            )) revert InvalidQuorumCertificate();

        // These are strictly not necessary, but we'll assert them here since they are cheap while
        // the prover doesn't (yet) enforce them.
        //   - continuity:  prevDepositNumber must equal where we last left off
        //   - monotonic:   the queue can only advance (nextDepositNumber >= prevDepositNumber)
        //   - in-range:    cannot process more deposits than have been enqueued
        if (
            depositQueueTransition.prevDepositNumber != lastProcessedDepositNumber
                || depositQueueTransition.nextDepositNumber < depositQueueTransition.prevDepositNumber
                || depositQueueTransition.nextDepositNumber > depositCount
        ) {
            revert InvalidDepositTransition();
        }

        uint64 enabledCount = uint64(_enabledTokens.length);
        //   - bootstrap:   before initialization, pre-T13 and the first T13 transition start at 0
        //   - continuity:  once initialized, prevProcessedTokenCount must equal where we last left off
        //   - monotonic:   the processed prefix can only advance
        //   - in-range:    cannot process more tokens than have been enabled
        uint64 expectedPrev = tokenEnablementCursorInitialized ? lastProcessedEnabledTokenCount : 0;
        if (
            tokenEnablementTransition.prevProcessedTokenCount != expectedPrev
                || tokenEnablementTransition.nextProcessedTokenCount < tokenEnablementTransition.prevProcessedTokenCount
                || tokenEnablementTransition.nextProcessedTokenCount > enabledCount
        ) {
            revert InvalidTokenEnablementTransition();
        }
        if (
            !tokenEnablementCursorInitialized && tokenEnablementTransition.nextProcessedTokenCount != 0
                && enabledCount - tokenEnablementTransition.nextProcessedTokenCount > MAX_UNPROCESSED_TOKEN_ENABLEMENTS
        ) {
            revert InvalidTokenEnablementTransition();
        }

        // Verify proof (handles both direct and ancestry modes)
        bool valid = IVerifier(verifier)
            .verify(
                zoneId,
                tempoBlockNumber,
                anchorBlockNumber,
                anchorBlockHash,
                withdrawalBatchIndex + 1,
                nextZoneHeight,
                blockTransition,
                depositQueueTransition,
                tokenEnablementTransition,
                withdrawalQueueHash,
                verifierConfig,
                proof
            );
        if (!valid) revert InvalidProof();

        // Update state
        withdrawalBatchIndex++;
        blockHash = blockTransition.nextBlockHash;
        lastSyncedTempoBlockNumber = tempoBlockNumber;
        lastProcessedDepositNumber = depositQueueTransition.nextDepositNumber;
        if (tokenEnablementCursorInitialized || tokenEnablementTransition.nextProcessedTokenCount != 0) {
            lastProcessedEnabledTokenCount = tokenEnablementTransition.nextProcessedTokenCount;
            tokenEnablementCursorInitialized = true;
        }
        zoneHeight = nextZoneHeight;

        uint256 assignedQueueIndex = _withdrawalQueue.enqueue(withdrawalQueueHash);

        // Emit event after state updates
        emit BatchSubmitted(
            withdrawalBatchIndex,
            assignedQueueIndex,
            depositQueueTransition.nextProcessedHash,
            blockHash,
            withdrawalQueueHash,
            lastProcessedDepositNumber,
            lastProcessedEnabledTokenCount
        );
    }

    function _verifySettlement(
        uint256 nextZoneHeight,
        uint64 tempoBlockNumber,
        uint64 anchorBlockNumber,
        bytes32 anchorBlockHash,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        TokenEnablementTransition calldata tokenEnablementTransition,
        bytes32 withdrawalQueueHash,
        bytes calldata verifierConfig,
        bytes[] memory signatures
    ) internal view returns (bool) {
        if (nextZoneHeight <= zoneHeight) return false;

        uint64 currentFastEpoch = fastEpoch;
        bool useFastAuthority = fastEpochActive();
        uint256 threshold;
        uint256 maximumSigners;
        bytes32 structHash;
        if (useFastAuthority) {
            // The T14 registry fixes a three-member roster and a two-member certificate.
            threshold = 2;
            maximumSigners = 2;
            if (signatures.length != threshold) return false;
            structHash = keccak256(
                abi.encode(
                    FAST_SETTLEMENT_ATTESTATION_TYPEHASH,
                    zoneId,
                    currentFastEpoch,
                    _fastEpochs[currentFastEpoch].rosterHash,
                    zoneHeight,
                    blockHash,
                    withdrawalBatchIndex,
                    nextZoneHeight,
                    withdrawalBatchIndex + 1,
                    verifier,
                    tempoBlockNumber,
                    anchorBlockNumber,
                    anchorBlockHash,
                    keccak256(abi.encode(blockTransition)),
                    keccak256(abi.encode(depositQueueTransition)),
                    keccak256(abi.encode(tokenEnablementTransition)),
                    withdrawalQueueHash,
                    keccak256(verifierConfig)
                )
            );
        } else {
            threshold = sequencerThreshold;
            maximumSigners = _sequencers.length;
            if (signatures.length < threshold || signatures.length > maximumSigners) return false;
            structHash = keccak256(
                abi.encode(
                    SETTLEMENT_ATTESTATION_TYPEHASH,
                    zoneId,
                    sequencerSetVersion,
                    nextZoneHeight,
                    withdrawalBatchIndex + 1,
                    verifier,
                    tempoBlockNumber,
                    anchorBlockNumber,
                    anchorBlockHash,
                    keccak256(abi.encode(blockTransition)),
                    keccak256(abi.encode(depositQueueTransition)),
                    keccak256(abi.encode(tokenEnablementTransition)),
                    withdrawalQueueHash,
                    keccak256(verifierConfig)
                )
            );
        }
        bytes32 digest = keccak256(abi.encodePacked("\x19\x01", _domainSeparator(), structHash));
        address[] memory recovered = new address[](signatures.length);

        for (uint256 i = 0; i < signatures.length; ++i) {
            bytes memory signature = signatures[i];
            address signer;
            // The shared TIP-1020 verifier owns signature-format and canonicality checks.
            // Convert its reverts into `false` so the public verifier remains non-reverting.
            try StdPrecompiles.SIGNATURE_VERIFIER.recover(digest, signature) returns (address recoveredSigner) {
                signer = recoveredSigner;
            } catch {
                return false;
            }
            if (
                signer == address(0)
                    || (useFastAuthority ? !_isFastEpochMember[currentFastEpoch][signer] : !isSequencer(signer))
            ) return false;
            for (uint256 j = 0; j < i; ++j) {
                if (recovered[j] == signer) return false;
            }
            recovered[i] = signer;
        }

        return signatures.length >= threshold;
    }

    function _domainSeparator() internal view returns (bytes32) {
        return keccak256(abi.encode(EIP712_DOMAIN_TYPEHASH, NAME_HASH, VERSION_HASH, block.chainid, address(this)));
    }
}
