//! `IZoneInbox` — Zone L2 system contract interface (0x1c00...0001).

pub use IZoneInbox::{
    ChaumPedersenProof, DecryptionData, DepositType, EnabledToken,
    IZoneInboxErrors as ZoneInboxError, IZoneInboxEvents as ZoneInboxEvent, QueuedDeposit,
    WithdrawalBounceBackDeposit,
};

use alloy_primitives::Address;

crate::sol! {
    #[derive(Debug, PartialEq, Eq)]
    contract IZoneInbox {
        // -- Shared types --

        struct WithdrawalBounceBackDeposit {
            address token;
            address sender;
            address to;
            uint128 amount;
            address tempoRefundRecipient;
            bytes32 memo;
        }

        /// A TIP-20 token enabled on L1 for bridging to the zone.
        struct EnabledToken {
            address token;
            string name;
            string symbol;
            string currency;
        }

        /// Entry types for the unified deposit queue.
        enum DepositType {
            /// Internal withdrawal bounce-back entry.
            WithdrawalBounceBack,
            /// User deposit with an encrypted recipient and memo.
            Deposit,
        }

        /// A user deposit or internal withdrawal bounce-back passed to `advanceTempo`.
        struct QueuedDeposit {
            DepositType depositType;
            bytes depositData;
        }

        /// Chaum-Pedersen proof for ECDH shared secret derivation.
        struct ChaumPedersenProof {
            bytes32 s;
            bytes32 c;
        }

        /// Decryption data provided by the sequencer for user deposits.
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

        event DepositFailed(
            bytes32 indexed depositHash,
            address indexed sender,
            address token,
            uint128 amount
        );

        event WithdrawalBounceBackProcessed(address indexed zoneFallbackRecipient, address token, uint128 amount);

        event WithdrawalBounceBackPending(address indexed zoneFallbackRecipient, address token, uint128 amount);

        event RefundClaimed(address indexed recipient, address indexed token, uint128 amount);

        /// Emitted when a TIP-20 token is enabled on the zone via advanceTempo.
        event TokenEnabled(address indexed token, string name, string symbol, string currency);

        error OnlySequencer();
        error InvalidDepositQueueHash();
        error InvalidWithdrawalBounceBack();
        error MissingDecryptionData();
        error ExtraDecryptionData();
        error InvalidSharedSecretProof();
        error Unauthorized();

        function processedDepositQueueHash() external view returns (bytes32);
        function processedDepositNumber() external view returns (uint64);
        function tempoPortal() external view returns (address);
        function tempoState() external view returns (address);
        function refunds(address token, address owner) external view returns (uint128);
        function claimRefund(address token) external returns (uint128 amount);

        function advanceTempo(
            bytes calldata header,
            QueuedDeposit[] calldata deposits,
            DecryptionData[] calldata decryptions,
            EnabledToken[] calldata enabledTokens
        ) external;
    }
}

impl EnabledToken {
    /// Build the event emitted after enabling this token on the zone.
    pub fn enabled_event(self) -> ZoneInboxEvent {
        ZoneInboxEvent::token_enabled(self.token, self.name, self.symbol, self.currency)
    }
}

impl WithdrawalBounceBackDeposit {
    /// Build the event emitted after processing a withdrawal bounce-back.
    pub fn withdrawal_bounce_back_processed_event(
        &self,
        fallback_recipient: Address,
    ) -> ZoneInboxEvent {
        ZoneInboxEvent::withdrawal_bounce_back_processed(
            fallback_recipient,
            self.token,
            self.amount,
        )
    }

    /// Build the event emitted after parking a failed withdrawal bounce-back.
    pub fn withdrawal_bounce_back_pending_event(
        &self,
        fallback_recipient: Address,
    ) -> ZoneInboxEvent {
        ZoneInboxEvent::withdrawal_bounce_back_pending(fallback_recipient, self.token, self.amount)
    }
}
