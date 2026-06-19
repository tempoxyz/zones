//! Generated contract bindings for the Tempo Zone protocol.
//!
//! All contracts and the structs/enums they share are emitted from a single
//! [`alloy_sol_types::sol!`] invocation: the macro can only resolve user-defined types
//! (e.g. [`Withdrawal`], [`QueuedDeposit`]) that are declared within the same invocation, and
//! several zone contracts reference the same types.

/// Internal macro that emits the full `sol!` block, placing `$($rpc_attr)*`
/// before every `contract` declaration. Called twice: once with `#[sol(rpc)]`
/// (when the `rpc` feature is active) and once with nothing.
macro_rules! define_abi {
    ($($rpc_attr:tt)*) => {
        alloy_sol_types::sol! {
    // ---------------------------------------------------------------
    //  Shared types
    // ---------------------------------------------------------------

    #[derive(Debug)]
    struct Withdrawal {
        address token;
        bytes32 senderTag;
        address to;
        uint128 amount;
        uint128 fee;
        bytes32 memo;
        uint64 gasLimit;
        address fallbackRecipient;
        bytes callbackData;
        bytes encryptedSender;
    }

    #[derive(Debug)]
    struct Deposit {
        address token;
        address sender;
        address to;
        uint128 amount;
        bytes32 memo;
    }

    /// Encrypted deposit payload (ECIES encrypted recipient and memo)
    #[derive(Debug)]
    struct EncryptedDepositPayload {
        bytes32 ephemeralPubkeyX;
        uint8 ephemeralPubkeyYParity;
        bytes ciphertext;
        bytes12 nonce;
        bytes16 tag;
    }

    /// Encrypted deposit stored in the queue
    #[derive(Debug)]
    struct EncryptedDeposit {
        address token;
        address sender;
        uint128 amount;
        uint256 keyIndex;
        EncryptedDepositPayload encrypted;
    }

    #[derive(Debug)]
    struct BlockTransition {
        bytes32 prevBlockHash;
        bytes32 nextBlockHash;
    }

    #[derive(Debug)]
    struct DepositQueueTransition {
        bytes32 prevProcessedHash;
        bytes32 nextProcessedHash;
        uint64 prevDepositNumber;
        uint64 nextDepositNumber;
    }

    #[derive(Debug)]
    struct LastBatch {
        bytes32 withdrawalQueueHash;
        uint64 withdrawalBatchIndex;
    }

    /// A TIP-20 token enabled on L1 for bridging to the zone.
    #[derive(Debug)]
    struct EnabledToken {
        address token;
        string name;
        string symbol;
        string currency;
    }

    /// Generic unauthorized access error used by zone wrapper logic.
    error Unauthorized();

    // ---------------------------------------------------------------
    //  ZonePortal — deployed on Tempo L1
    // ---------------------------------------------------------------

        $($rpc_attr)*
    contract ZonePortal {
        // -- Events --

        #[derive(Debug)]
        event DepositMade(
            bytes32 indexed newCurrentDepositQueueHash,
            address indexed sender,
            address token,
            address to,
            uint128 netAmount,
            uint128 fee,
            bytes32 memo,
            uint64 depositNumber
        );

        #[derive(Debug)]
        event EncryptedDepositMade(
            bytes32 indexed newCurrentDepositQueueHash,
            address indexed sender,
            address token,
            uint128 netAmount,
            uint128 fee,
            uint256 keyIndex,
            bytes32 ephemeralPubkeyX,
            uint8 ephemeralPubkeyYParity,
            bytes ciphertext,
            bytes12 nonce,
            bytes16 tag,
            uint64 depositNumber
        );

        /// Event emitted when a new TIP-20 token is enabled for bridging.
        /// Includes token metadata so the zone can create a matching TIP-20.
        #[derive(Debug)]
        event TokenEnabled(address indexed token, string name, string symbol, string currency);

        #[derive(Debug)]
        event BatchSubmitted(
            uint64 indexed withdrawalBatchIndex,
            bytes32 nextProcessedDepositQueueHash,
            bytes32 nextBlockHash,
            bytes32 withdrawalQueueHash,
            uint64 lastProcessedDepositNumber
        );

        #[derive(Debug)]
        event WithdrawalProcessed(address indexed to, address token, uint128 amount, bool callbackSuccess);

        #[derive(Debug)]
        event BounceBack(
            bytes32 indexed newCurrentDepositQueueHash,
            address indexed fallbackRecipient,
            address token,
            uint128 amount,
            uint64 depositNumber
        );

        #[derive(Debug)]
        event SequencerTransferStarted(
            address indexed currentSequencer,
            address indexed pendingSequencer
        );

        #[derive(Debug)]
        event SequencerTransferred(
            address indexed previousSequencer,
            address indexed newSequencer
        );

        // -- Errors --

        #[derive(Debug)]
        error NotSequencer();
        #[derive(Debug)]
        error InvalidProof();
        #[derive(Debug)]
        error InvalidTempoBlockNumber();
        #[derive(Debug)]
        error DepositPolicyForbids();

        // -- View functions --

        function zoneId() external view returns (uint32);
        function sequencer() external view returns (address);
        function verifier() external view returns (address);
        function sequencerPubkey() external view returns (bytes32);
        function withdrawalBatchIndex() external view returns (uint64);
        function blockHash() external view returns (bytes32);
        function currentDepositQueueHash() external view returns (bytes32);
        function lastSyncedTempoBlockNumber() external view returns (uint64);
        function withdrawalQueueHead() external view returns (uint256);
        function withdrawalQueueTail() external view returns (uint256);
        function withdrawalQueueMaxSize() external view returns (uint256);
        function withdrawalQueueSlot(uint256 slot) external view returns (bytes32);
        function genesisTempoBlockNumber() external view returns (uint64);
        function calculateDepositFee() external view returns (uint128 fee);
        function depositCount() external view returns (uint64);
        function lastProcessedDepositNumber() external view returns (uint64);
        function MAX_WITHDRAWAL_GAS_LIMIT() external view returns (uint64);

        // -- State-changing functions --

        function deposit(address token, address to, uint128 amount, bytes32 memo)
            external
            returns (bytes32 newCurrentDepositQueueHash);

        function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external;

        function submitBatch(
            uint64 tempoBlockNumber,
            uint64 recentTempoBlockNumber,
            BlockTransition calldata blockTransition,
            DepositQueueTransition calldata depositQueueTransition,
            bytes32 withdrawalQueueHash,
            bytes calldata verifierConfig,
            bytes calldata proof
        ) external;

        function enableToken(address token) external;

        function rpcUrl() external view returns (string memory);
        function setRpcUrl(string calldata rpcUrl) external;

        function depositEncrypted(
            address token,
            uint128 amount,
            uint256 keyIndex,
            EncryptedDepositPayload calldata encrypted
        ) external returns (bytes32 newCurrentDepositQueueHash);

        function setSequencerEncryptionKey(
            bytes32 x,
            uint8 yParity,
            uint8 popV,
            bytes32 popR,
            bytes32 popS
        ) external;

        // -- View functions (token management) --

        function isTokenEnabled(address token) external view returns (bool);
        function enabledTokenCount() external view returns (uint256);
        function enabledTokenAt(uint256 index) external view returns (address);
        function zoneGasRate() external view returns (uint128);
        function pendingSequencer() external view returns (address);

        function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);

        function encryptionKeyCount() external view returns (uint256);
    }

    // ---------------------------------------------------------------
    //  ZoneOutbox — deployed on Zone L2
    // ---------------------------------------------------------------

        $($rpc_attr)*
    contract ZoneOutbox {
        // -- Events --

        event WithdrawalRequested(
            uint64 indexed withdrawalIndex,
            address indexed sender,
            address token,
            address to,
            uint128 amount,
            uint128 fee,
            bytes32 memo,
            uint64 gasLimit,
            address fallbackRecipient,
            bytes data,
            bytes revealTo
        );

        #[derive(Debug)]
        event BatchFinalized(bytes32 indexed withdrawalQueueHash, uint64 withdrawalBatchIndex);

        // -- Errors --

        error OnlySequencer();
        error GasLimitTooHigh();

        // -- View functions --

        function lastBatch() external view returns (LastBatch memory);
        function withdrawalBatchIndex() external view returns (uint64);
        function nextWithdrawalIndex() external view returns (uint64);
        function pendingWithdrawalsCount() external view returns (uint256);
        function calculateWithdrawalFee(uint64 gasLimit) external view returns (uint128 fee);
        function MAX_WITHDRAWAL_GAS_LIMIT() external view returns (uint64);

        // -- State-changing functions --

        function requestWithdrawal(
            address token,
            address to,
            uint128 amount,
            bytes32 memo,
            uint64 gasLimit,
            address fallbackRecipient,
            bytes calldata data,
            bytes calldata revealTo
        ) external;
        function finalizeWithdrawalBatch(uint256 count, uint64 blockNumber, bytes[] calldata encryptedSenders) external returns (bytes32 withdrawalQueueHash);
    }

    // ---------------------------------------------------------------
    //  TempoState — Zone L2 predeploy (0x1c00...0000)
    // ---------------------------------------------------------------

        $($rpc_attr)*
    contract TempoState {
        #[derive(Debug)]
        event TempoBlockFinalized(bytes32 indexed blockHash, uint64 indexed blockNumber, bytes32 stateRoot);

        error InvalidParentHash();
        error InvalidBlockNumber();
        error InvalidRlpData();
        error OnlyZoneInbox();

        function tempoBlockHash() external view returns (bytes32);
        function tempoBlockNumber() external view returns (uint64);
        function tempoStateRoot() external view returns (bytes32);
        function tempoParentHash() external view returns (bytes32);
        function tempoBeneficiary() external view returns (address);
        function tempoTransactionsRoot() external view returns (bytes32);
        function tempoReceiptsRoot() external view returns (bytes32);
        function tempoGasLimit() external view returns (uint64);
        function tempoGasUsed() external view returns (uint64);
        function tempoTimestamp() external view returns (uint64);
        function tempoTimestampMillis() external view returns (uint64);
        function tempoPrevRandao() external view returns (bytes32);
        function generalGasLimit() external view returns (uint64);
        function sharedGasLimit() external view returns (uint64);

        function finalizeTempo(bytes calldata header) external;
    }

    // ---------------------------------------------------------------
    //  TempoStateReader — Zone L2 standalone precompile
    //  Separate from TempoState; reads Tempo L1 storage at a caller-specified block.
    // ---------------------------------------------------------------

        $($rpc_attr)*
    contract TempoStateReader {
        error DelegateCallNotAllowed();

        function readStorageAt(address account, bytes32 slot, uint64 blockNumber) external view returns (bytes32);
        function readStorageBatchAt(address account, bytes32[] calldata slots, uint64 blockNumber) external view returns (bytes32[] memory);
    }

        $($rpc_attr)*
    contract ZoneTxContext {
        function currentTxHash() external returns (bytes32);
    }

    // ---------------------------------------------------------------
    //  ZoneInbox shared types — Zone L2 system contract (0x1c00...0001)
    // ---------------------------------------------------------------

    /// Deposit types for the unified deposit queue.
    #[derive(Debug, PartialEq, Eq)]
    enum DepositType {
        Regular,
        Encrypted,
    }

    /// A queued deposit (regular or encrypted) passed to `advanceTempo`.
    #[derive(Debug)]
    struct QueuedDeposit {
        DepositType depositType;
        bytes depositData;
    }

    /// Chaum-Pedersen proof for ECDH shared secret derivation.
    #[derive(Debug)]
    struct ChaumPedersenProof {
        bytes32 s;
        bytes32 c;
    }

    /// Decryption data provided by the sequencer for encrypted deposits.
    #[derive(Debug)]
    struct DecryptionData {
        bytes32 sharedSecret;
        uint8 sharedSecretYParity;
        ChaumPedersenProof cpProof;
    }

    // ---------------------------------------------------------------
    //  ZoneFactory — deployed on Tempo L1
    // ---------------------------------------------------------------

    #[derive(Debug)]
    struct ZoneInfo {
        uint32 zoneId;
        address portal;
        address messenger;
        address initialToken;
        address sequencer;
        address verifier;
        bytes32 genesisBlockHash;
        bytes32 genesisTempoBlockHash;
        uint64 genesisTempoBlockNumber;
        string rpcUrl;
    }

        $($rpc_attr)*
    contract ZoneFactory {
        struct ZoneParams {
            bytes32 genesisBlockHash;
            bytes32 genesisTempoBlockHash;
            uint64 genesisTempoBlockNumber;
        }
        struct CreateZoneParams {
            address token;
            address sequencer;
            address verifier;
            ZoneParams zoneParams;
            string rpcUrl;
        }
        #[derive(Debug)]
        event ZoneCreated(
            uint32 indexed zoneId,
            address indexed portal,
            address indexed messenger,
            address token,
            address sequencer,
            address verifier,
            bytes32 genesisBlockHash,
            bytes32 genesisTempoBlockHash,
            uint64 genesisTempoBlockNumber
        );
        function createZone(CreateZoneParams calldata params) external returns (uint32 zoneId, address portal);
        function verifier() external view returns (address);
        function zones(uint32 zoneId) external view returns (ZoneInfo memory);
        function zoneCount() external view returns (uint32);
        function isZonePortal(address portal) external view returns (bool);
        function isZoneMessenger(address messenger) external view returns (bool);
    }

    // ---------------------------------------------------------------
    //  ZoneInbox — Zone L2 system contract (0x1c00...0001)
    // ---------------------------------------------------------------

        $($rpc_attr)*
    contract ZoneInbox {
        #[derive(Debug)]
        event TempoAdvanced(
            bytes32 indexed tempoBlockHash,
            uint64 indexed tempoBlockNumber,
            uint256 depositsProcessed,
            bytes32 newProcessedDepositQueueHash,
            uint64 lastProcessedDepositNumber
        );

        #[derive(Debug)]
        event DepositProcessed(
            bytes32 indexed depositHash,
            address indexed sender,
            address indexed to,
            address token,
            uint128 amount,
            bytes32 memo
        );

        #[derive(Debug)]
        event EncryptedDepositProcessed(
            bytes32 indexed depositHash,
            address indexed sender,
            address indexed to,
            address token,
            uint128 amount,
            bytes32 memo
        );

        #[derive(Debug)]
        event EncryptedDepositFailed(
            bytes32 indexed depositHash,
            address indexed sender,
            address token,
            uint128 amount
        );

        /// Emitted when a TIP-20 token is enabled on the zone via advanceTempo.
        #[derive(Debug)]
        event TokenEnabled(address indexed token, string name, string symbol, string currency);

        error OnlySequencer();
        error InvalidDepositQueueHash();
        error MissingDecryptionData();
        error ExtraDecryptionData();
        error InvalidSharedSecretProof();
        function processedDepositQueueHash() external view returns (bytes32);
        function processedDepositNumber() external view returns (uint64);
        function tempoPortal() external view returns (address);
        function tempoState() external view returns (address);
        function config() external view returns (address);

        function advanceTempo(
            bytes calldata header,
            QueuedDeposit[] calldata deposits,
            DecryptionData[] calldata decryptions,
            EnabledToken[] calldata enabledTokens
        ) external;
    }

    // ---------------------------------------------------------------
    //  SwapAndDepositRouter — deployed on Tempo L1
    // ---------------------------------------------------------------

        $($rpc_attr)*
    contract SwapAndDepositRouter {
        function onWithdrawalReceived(
            bytes32 senderTag,
            address tokenIn,
            uint128 amount,
            bytes calldata data
        ) external returns (bytes4);
    }
        }
    };
}

#[cfg(feature = "rpc")]
define_abi!(#[sol(rpc)]);

#[cfg(not(feature = "rpc"))]
define_abi!();
