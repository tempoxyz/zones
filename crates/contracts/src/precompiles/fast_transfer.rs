//! ABI for the Zone-native `FastTransfer` precompile.
//!
//! Consensus-relevant intents and certificates are passed as the canonical bounded byte encoding
//! from `zone-primitives`.  The ABI deliberately does not mirror those structs: maintaining a
//! second Solidity tuple was the source of a different transfer identity at the EVM boundary.

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes, address};
use alloy_sol_types::SolCall;
use zone_primitives::fast_transfer::{FastBarrierStatement, SignatureBytes};

/// Reserved Zone address for the native fast-transfer state machine.
pub const FAST_TRANSFER_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000003");

/// Version byte for the fixed-width source-quorum certificate accepted by
/// `recordImportedBarrier`.
pub const IMPORTED_BARRIER_CERTIFICATE_VERSION: u8 = 1;
/// `version || digest || signature[0] || signature[1]`.
pub const IMPORTED_BARRIER_CERTIFICATE_BYTES: usize = 1 + 32 + 65 + 65;

/// Exact target, calldata and successful return value for a protocol-native imported-barrier
/// transaction. The caller must submit this as an ordinary transaction to
/// [`FAST_TRANSFER_ADDRESS`]; it is not a system or no-op transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedBarrierCall {
    pub target: Address,
    pub calldata: Bytes,
    pub destination_epoch: u64,
    pub source_portal: Address,
    pub source_epoch: u64,
    /// Signed registry digest returned by native execution and stored permanently.
    pub barrier_digest: B256,
    /// Signed complete-lock root. The retained full inventory must reconstruct this root.
    pub complete_lock_root: B256,
}

impl ImportedBarrierCall {
    /// Validate the deterministic ABI return bytes of a successful native execution. Receipt
    /// status must be checked separately by the committed-block consumer.
    pub fn validate_return(&self, output: &[u8]) -> bool {
        IFastTransfer::recordImportedBarrierCall::abi_decode_returns(output)
            .is_ok_and(|value| value == self.barrier_digest)
    }
}

/// Construct the canonical protocol-native transaction for one authenticated imported barrier.
///
/// The full inventory is deliberately absent from EVM calldata. It remains in the fsynced Raft
/// protocol record, where the consumer reconstructs this statement's signed roots before
/// acknowledging the import. Native execution authenticates the compact statement itself and
/// returns its source-quorum digest.
pub fn imported_barrier_call(
    statement: &FastBarrierStatement,
    certificate_digest: B256,
    signatures: [SignatureBytes; 2],
) -> ImportedBarrierCall {
    let mut certificate = Vec::with_capacity(IMPORTED_BARRIER_CERTIFICATE_BYTES);
    certificate.push(IMPORTED_BARRIER_CERTIFICATE_VERSION);
    certificate.extend_from_slice(certificate_digest.as_slice());
    certificate.extend_from_slice(&signatures[0].0);
    certificate.extend_from_slice(&signatures[1].0);
    let calldata = IFastTransfer::recordImportedBarrierCall {
        canonicalInventory: source_barrier_statement(statement),
        sourceBarrierCertificate: certificate.into(),
    }
    .abi_encode()
    .into();
    ImportedBarrierCall {
        target: FAST_TRANSFER_ADDRESS,
        calldata,
        destination_epoch: statement.destination_epoch,
        source_portal: statement.source_portal,
        source_epoch: statement.source_epoch,
        barrier_digest: certificate_digest,
        complete_lock_root: statement.complete_lock_root,
    }
}

/// Project the shared Rust statement into the exact Solidity/native ABI tuple.
pub fn source_barrier_statement(
    statement: &FastBarrierStatement,
) -> IFastTransfer::SourceBarrierStatement {
    IFastTransfer::SourceBarrierStatement {
        destinationPortal: statement.destination_portal,
        destinationEpoch: statement.destination_epoch,
        closureHash: statement.closure_hash,
        sourcePortal: statement.source_portal,
        sourceEpoch: statement.source_epoch,
        importedAnchorNumber: statement.imported_anchor_number,
        importedAnchorHash: statement.imported_anchor_hash,
        logTerm: statement.log_term,
        logIndex: statement.log_index,
        blockHeight: statement.block_height,
        blockHash: statement.block_hash,
        stateRoot: statement.state_root,
        lockLogWatermark: statement.lock_log_watermark,
        completeLockRoot: statement.complete_lock_root,
        unresolvedRoot: statement.unresolved_root,
        unresolvedCount: statement.unresolved_count,
    }
}

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

        struct InventoryJob {
            bytes32 intentHash;
            address operator;
            address token;
            address treasury;
            uint128 amount;
            uint64 fallbackNonce;
            uint64 withdrawalIndex;
            bool restored;
        }

        /// Compact source-quorum statement. The complete inventory is retained in the Raft
        /// protocol record and must reconstruct these signed roots before this call is submitted.
        struct SourceBarrierStatement {
            address destinationPortal;
            uint64 destinationEpoch;
            bytes32 closureHash;
            address sourcePortal;
            uint64 sourceEpoch;
            uint64 importedAnchorNumber;
            bytes32 importedAnchorHash;
            uint64 logTerm;
            uint64 logIndex;
            uint256 blockHeight;
            bytes32 blockHash;
            bytes32 stateRoot;
            uint64 lockLogWatermark;
            bytes32 completeLockRoot;
            bytes32 unresolvedRoot;
            uint64 unresolvedCount;
        }

        event Locked(bytes32 indexed transferId, bytes32 indexed intentHash, address indexed sender, address token, uint128 total);
        event Paid(bytes32 indexed transferId, bytes32 indexed intentHash, address indexed recipient, address token, uint128 principal);
        event Rejected(bytes32 indexed transferId, bytes32 indexed intentHash, uint8 reason);
        event OutcomeRecorded(bytes32 indexed transferId, uint8 outcome, address beneficiary);
        event EscrowDisposed(bytes32 indexed transferId, uint8 outcome, address indexed beneficiary, uint128 amount);
        event PoolFunded(address indexed token, address indexed operator, uint128 amount);
        event PoolWithdrawn(address indexed token, address indexed operator, address indexed recipient, uint128 amount);
        event ExposureRetired(bytes32 indexed transferId, bytes32 indexed sourceZone, address indexed token, uint128 principal);
        event ReplenishmentRouteConfigured(address indexed token, address indexed treasury, address indexed operator, bool enabled);
        event InventoryAllocated(bytes32 indexed jobId, bytes32 indexed intentHash, address indexed operator, address token, address treasury, uint128 amount, uint64 fallbackNonce, uint64 withdrawalIndex);
        event InventoryRestored(bytes32 indexed jobId, uint128 amount);
        event ReplenishmentCredited(bytes32 indexed jobId, address indexed token, address indexed operator, uint128 amount);
        event ImportedBarrierRecorded(uint64 indexed destinationEpoch, address indexed sourcePortal, uint64 indexed sourceEpoch, bytes32 barrierDigest, bytes32 completeLockRoot);

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
        error InvalidInventoryJob();
        error InventoryJobConflict();
        error DuplicateInventoryContribution();
        error InventoryNotReleased(bytes32 transferId);
        error InventoryContributionMismatch(bytes32 transferId);
        error InventoryAlreadyAllocated(bytes32 transferId);
        error InventoryRestorationMismatch();
        error InvalidImportedBarrier();
        error ImportedBarrierConflict();

        function lock(bytes calldata canonicalIntent, bytes calldata quoteCertificate) external returns (uint8 state);
        function resolve(bytes calldata canonicalIntent, bytes calldata lockCertificate, bytes calldata cancellation, bytes calldata barrierProof) external returns (uint8 state);
        function recordOutcome(bytes calldata canonicalIntent, bytes calldata outcomeCertificate) external returns (uint8 state);
        function disposeEscrow(bytes32 transferId) external returns (uint8 state);
        function fundPool(address token, uint128 amount, uint128 minimumReserve) external;
        function withdrawPool(address token, address recipient, uint128 amount) external;
        function setExposureLimit(address token, bytes32 sourceZone, uint128 limit) external;
        function recordAncestryCheckpoint(address sourcePortal, bytes calldata headerChain) external;
        function retireExposure(bytes calldata canonicalEvidence) external;
        function configureReplenishmentRoute(address token, address treasury, bool enabled) external;
        function allocateInventoryAndWithdraw(bytes32 jobId, bytes32[] calldata transferIds, address token, address treasury) external returns (uint128 amount, uint64 fallbackNonce, uint64 withdrawalIndex);
        function recordImportedBarrier(SourceBarrierStatement calldata canonicalInventory, bytes calldata sourceBarrierCertificate) external returns (bytes32 barrierDigest);
        function importedBarrier(uint64 destinationEpoch, address sourcePortal, uint64 sourceEpoch) external view returns (bytes32 barrierDigest);
        function status(bytes32 transferId) external view returns (FastTransferStatus memory);
        function poolState(address token) external view returns (PoolState memory);
        function exposure(address token, bytes32 sourceZone) external view returns (uint128 unsettled, uint128 limit);
        function inventoryJob(bytes32 jobId) external view returns (InventoryJob memory job);
        function replenishmentRoute(address token, address treasury) external view returns (address poolOperator);
        function replenishmentCredit(bytes32 jobId) external view returns (uint128 amount);
        function MAX_CERTIFICATE_BYTES() external pure returns (uint256);
        function MAX_RETIREMENT_PROOF_BYTES() external pure returns (uint256);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use alloy_sol_types::SolValue;

    #[test]
    fn imported_barrier_builder_has_exact_bounded_calldata_and_signed_commitment() {
        let statement = FastBarrierStatement {
            destination_portal: Address::repeat_byte(1),
            destination_epoch: 2,
            closure_hash: B256::repeat_byte(3),
            source_portal: Address::repeat_byte(4),
            source_epoch: 5,
            imported_anchor_number: 6,
            imported_anchor_hash: B256::repeat_byte(7),
            log_term: 8,
            log_index: 10_000,
            block_height: U256::from(11),
            block_hash: B256::repeat_byte(12),
            state_root: B256::repeat_byte(13),
            lock_log_watermark: 10_000,
            complete_lock_root: B256::repeat_byte(14),
            unresolved_root: B256::repeat_byte(15),
            unresolved_count: 9_999,
        };
        let digest = B256::repeat_byte(9);
        let call = imported_barrier_call(
            &statement,
            digest,
            [SignatureBytes([7; 65]), SignatureBytes([8; 65])],
        );
        assert_eq!(call.target, FAST_TRANSFER_ADDRESS);
        assert_eq!(call.destination_epoch, statement.destination_epoch);
        assert_eq!(call.source_portal, statement.source_portal);
        assert_eq!(call.source_epoch, statement.source_epoch);
        assert_eq!(call.barrier_digest, digest);
        assert_eq!(call.complete_lock_root, statement.complete_lock_root);
        assert_eq!(call.calldata.len(), 772);
        let decoded = IFastTransfer::recordImportedBarrierCall::abi_decode(&call.calldata)
            .expect("canonical imported-barrier calldata");
        assert_eq!(
            decoded.canonicalInventory,
            source_barrier_statement(&statement)
        );
        assert_eq!(
            decoded.sourceBarrierCertificate.len(),
            IMPORTED_BARRIER_CERTIFICATE_BYTES
        );
        assert_eq!(
            decoded.sourceBarrierCertificate[0],
            IMPORTED_BARRIER_CERTIFICATE_VERSION
        );
        assert_eq!(&decoded.sourceBarrierCertificate[1..33], digest.as_slice());
        assert_eq!(&decoded.sourceBarrierCertificate[33..98], &[7; 65]);
        assert_eq!(&decoded.sourceBarrierCertificate[98..163], &[8; 65]);
        assert!(call.validate_return(&call.barrier_digest.abi_encode()));
        assert!(!call.validate_return(&B256::repeat_byte(1).abi_encode()));
    }
}
