//! `ZoneInbox` — Zone L2 system contract (0x1c00...0001).

pub use ZoneInbox::{
    ChaumPedersenProof, DecryptionData, Deposit, DepositType, EnabledToken, QueuedDeposit,
};

crate::sol! {
    #[derive(Debug, PartialEq, Eq)]
    contract ZoneInbox {
        // -- Shared types --

        struct Deposit {
            address token;
            address sender;
            address to;
            uint128 amount;
            address bouncebackRecipient;
            bytes32 memo;
        }

        /// A TIP-20 token enabled on L1 for bridging to the zone.
        struct EnabledToken {
            address token;
            string name;
            string symbol;
            string currency;
        }

        /// Deposit types for the unified deposit queue.
        enum DepositType {
            Regular,
            Encrypted,
        }

        /// A queued deposit (regular or encrypted) passed to `advanceTempo`.
        struct QueuedDeposit {
            DepositType depositType;
            bytes depositData;
        }

        /// Chaum-Pedersen proof for ECDH shared secret derivation.
        struct ChaumPedersenProof {
            bytes32 s;
            bytes32 c;
        }

        /// Decryption data provided by the sequencer for encrypted deposits.
        struct DecryptionData {
            bytes32 sharedSecret;
            uint8 sharedSecretYParity;
            ChaumPedersenProof cpProof;
        }

        // -- Events --

        event TempoAdvanced(
            bytes32 indexed tempoBlockHash,
            uint64 indexed tempoBlockNumber,
            uint256 depositsProcessed,
            bytes32 newProcessedDepositQueueHash,
            uint64 lastProcessedDepositNumber
        );

        event DepositProcessed(
            bytes32 indexed depositHash,
            address indexed sender,
            address indexed to,
            address token,
            uint128 amount,
            bytes32 memo
        );

        event EncryptedDepositProcessed(
            bytes32 indexed depositHash,
            address indexed sender,
            address indexed to,
            address token,
            uint128 amount,
            bytes32 memo
        );

        event EncryptedDepositFailed(
            bytes32 indexed depositHash,
            address indexed sender,
            address token,
            uint128 amount
        );

        /// Emitted when a TIP-20 token is enabled on the zone via advanceTempo.
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
}
