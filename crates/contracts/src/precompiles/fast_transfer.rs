//! ABI for the dormant Zone-native `FastTransfer` precompile.
//!
//! The ABI is intentionally fixed-width except for certificate and ancestry proof bytes. Native
//! execution applies strict bounds to those byte strings before verification.

use alloy_primitives::{Address, address};

/// Reserved Zone address for the native fast-transfer state machine.
pub const FAST_TRANSFER_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000003");

pub use IFastTransfer::{
    Certificate, FastTransferStatus, IFastTransferErrors as FastTransferError,
    IFastTransferEvents as FastTransferEvent, Intent, PoolState, RetirementEvidence,
};

crate::sol! {
    #[sol(abi)]
    #[derive(Debug, PartialEq, Eq)]
    contract IFastTransfer {
        /// Complete immutable user intent. `refundAccount` MUST equal `sender` in version one.
        struct Intent {
            bytes32 transferId;
            bytes32 sourceZone;
            bytes32 destinationZone;
            uint64 sourceChainId;
            uint64 destinationChainId;
            address sourcePortal;
            address destinationPortal;
            uint64 sourceEpoch;
            uint64 destinationEpoch;
            uint32 protocolVersion;
            address sourceToken;
            address destinationToken;
            address sender;
            address recipient;
            address refundAccount;
            address pool;
            address reimbursementAccount;
            uint128 principal;
            uint128 fee;
            bytes32 quoteId;
            uint64 destinationExpiryHeight;
            uint64 transferNonce;
        }

        /// Quorum certificate. Signatures are canonical 65-byte ECDSA values concatenated in
        /// signer-address order; the native verifier accepts exactly two signatures.
        struct Certificate {
            uint8 outcome;
            bytes32 transferId;
            bytes32 intentHash;
            bytes32 zoneDomain;
            bytes32 rosterHash;
            uint64 epoch;
            uint64 logTerm;
            uint64 logIndex;
            uint64 blockHeight;
            bytes32 blockHash;
            bytes32 stateRoot;
            bytes32 transactionHash;
            address token;
            address pool;
            address beneficiary;
            uint128 principal;
            uint128 total;
            bytes signatures;
        }

        /// Finalized source-release inclusion and ancestry proof used to retire exposure.
        struct RetirementEvidence {
            bytes32 transferId;
            bytes32 intentHash;
            bytes32 acceptedSourceBlockHash;
            bytes32 releaseReceiptHash;
            address token;
            address beneficiary;
            uint128 amount;
            bytes receiptProof;
            bytes headerChain;
        }

        struct FastTransferStatus {
            bytes32 intentHash;
            uint8 state;
            address token;
            address beneficiary;
            uint128 principal;
            uint128 total;
            bool exposureRetired;
        }

        struct PoolState {
            address operator;
            uint128 fundedBalance;
            uint128 minimumReserve;
        }

        event Locked(bytes32 indexed transferId, bytes32 indexed intentHash, address indexed sender, address token, uint128 total);
        event Paid(bytes32 indexed transferId, bytes32 indexed intentHash, address indexed recipient, address token, uint128 principal);
        event Rejected(bytes32 indexed transferId, bytes32 indexed intentHash, uint8 reason);
        event OutcomeRecorded(bytes32 indexed transferId, uint8 outcome, address beneficiary);
        event EscrowDisposed(bytes32 indexed transferId, uint8 outcome, address indexed beneficiary, uint128 amount);
        event PoolFunded(address indexed token, address indexed operator, uint128 amount);
        event PoolWithdrawn(address indexed token, address indexed operator, address indexed recipient, uint128 amount);
        event ExposureRetired(bytes32 indexed transferId, bytes32 indexed sourceZone, address indexed token, uint128 principal);

        error FastTransferNotActive();
        error StaticCallNotAllowed();
        error InvalidIntent();
        error IntentMismatch();
        error NonceAlreadyUsed();
        error InvalidState(uint8 actual);
        error InvalidCertificate();
        error CertificateTooLarge();
        error InvalidRetirementEvidence();
        error RetirementProofTooLarge();
        error Unauthorized();
        error AccountNotAllowed(address account);
        error TransferFailed();
        error InsufficientPoolLiquidity();
        error ExposureLimitExceeded();
        error ArithmeticOverflow();

        function lock(Intent calldata intent, bytes32 intentHash) external returns (uint8 state);
        function resolve(Intent calldata intent, bytes32 intentHash, Certificate calldata lockCertificate, bool cancel, uint8 rejectionReason) external returns (uint8 state);
        function recordOutcome(Intent calldata intent, bytes32 intentHash, Certificate calldata outcomeCertificate) external returns (uint8 state);
        function disposeEscrow(bytes32 transferId) external returns (uint8 state);
        function fundPool(address token, uint128 amount, uint128 minimumReserve) external;
        function withdrawPool(address token, address recipient, uint128 amount) external;
        function setExposureLimit(address token, bytes32 sourceZone, uint128 limit) external;
        function retireExposure(RetirementEvidence calldata evidence) external;
        function status(bytes32 transferId) external view returns (FastTransferStatus memory);
        function poolState(address token) external view returns (PoolState memory);
        function exposure(address token, bytes32 sourceZone) external view returns (uint128 unsettled, uint128 limit);
        function MAX_CERTIFICATE_BYTES() external pure returns (uint256);
        function MAX_RETIREMENT_PROOF_BYTES() external pure returns (uint256);
    }
}
