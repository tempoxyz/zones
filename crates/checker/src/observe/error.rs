//! Typed failures at the authenticated observation boundary.

use std::fmt;

use alloy_primitives::{Address, B256, keccak256};

use crate::observe::events::ProtocolEventError;

/// External or notification-local source required for a complete view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AcquisitionSource {
    #[error("Tempo block")]
    L1Block,
    #[error("Tempo receipts")]
    L1Receipts,
    #[error("Tempo transactions")]
    L1Transaction,
    #[error("Zone notification receipts")]
    ZoneNotificationReceipts,
    #[error("Zone notification block data")]
    ZoneNotificationBlock,
    #[error("Zone state")]
    ExactZoneState,
    #[error("Portal collateral")]
    PortalCollateral,
}

/// Location of an envelope violation without inventing transaction zero for
/// block-level failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum EnvelopeLocation {
    #[error("at block level")]
    Block,
    #[error("at transaction {0}")]
    Transaction(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProtocolChain {
    #[error("Tempo L1")]
    TempoL1,
    #[error("Zone L2")]
    ZoneL2,
}

/// Exact authenticated transaction containing a malformed protocol byte
/// surface. Observation wrappers attach this after the byte decoder returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{chain} transaction {transaction_index} ({transaction_hash})")]
pub(crate) struct AuthenticatedTransaction {
    chain: ProtocolChain,
    transaction_index: usize,
    transaction_hash: B256,
}

impl AuthenticatedTransaction {
    pub(crate) const fn new(
        chain: ProtocolChain,
        transaction_index: usize,
        transaction_hash: B256,
    ) -> Self {
        Self {
            chain,
            transaction_index,
            transaction_hash,
        }
    }

    pub(crate) const fn chain(self) -> ProtocolChain {
        self.chain
    }

    pub(crate) const fn transaction_index(self) -> usize {
        self.transaction_index
    }
}

/// Stable digest of the authenticated bytes that failed strict decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthenticatedDataEvidence {
    length: u64,
    hash: B256,
}

impl AuthenticatedDataEvidence {
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            length: u64::try_from(bytes.len()).expect("slice length must fit u64"),
            hash: keccak256(bytes),
        }
    }

    pub(crate) const fn length(self) -> u64 {
        self.length
    }

    pub(crate) const fn digest(self) -> B256 {
        self.hash
    }
}

/// Failure to acquire a complete, internally consistent view.
///
/// No variant is converted into a default observation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AcquisitionError {
    #[error("{kind} is unavailable: {detail}")]
    Unavailable {
        kind: AcquisitionSource,
        detail: String,
    },
    #[error("{kind} is missing: {identity}")]
    Missing {
        kind: AcquisitionSource,
        identity: String,
    },
    #[error("inconsistent {kind}: expected {expected}, got {actual}")]
    Inconsistent {
        kind: AcquisitionSource,
        expected: String,
        actual: String,
    },
}

impl AcquisitionError {
    pub(crate) fn unavailable(kind: AcquisitionSource, error: impl fmt::Display) -> Self {
        Self::Unavailable {
            kind,
            detail: error.to_string(),
        }
    }

    pub(crate) fn missing(kind: AcquisitionSource, identity: impl fmt::Display) -> Self {
        Self::Missing {
            kind,
            identity: identity.to_string(),
        }
    }

    pub(crate) fn inconsistent(
        kind: AcquisitionSource,
        expected: impl fmt::Display,
        actual: impl fmt::Display,
    ) -> Self {
        Self::Inconsistent {
            kind,
            expected: expected.to_string(),
            actual: actual.to_string(),
        }
    }
}

pub(crate) fn ensure_acquisition_equal<T>(
    source: AcquisitionSource,
    field: impl fmt::Display,
    expected: T,
    actual: T,
) -> Result<(), ObservationError>
where
    T: PartialEq + fmt::Debug,
{
    if expected != actual {
        return Err(AcquisitionError::inconsistent(
            source,
            format!("{field} {expected:?}"),
            format!("{field} {actual:?}"),
        )
        .into());
    }
    Ok(())
}

/// Protocol envelope rule enforced directly from canonical L2 data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum EnvelopeRule {
    #[error("only non-genesis blocks have observation envelopes")]
    NonGenesis,
    #[error("advanceTempo is missing")]
    AdvancePresent,
    #[error("advanceTempo caller is not the protocol system caller")]
    AdvanceSystemCaller,
    #[error("advanceTempo destination is not ZoneInbox")]
    AdvanceDestination,
    #[error("advanceTempo receipt is unsuccessful")]
    AdvanceSuccess,
    #[error("zero-sender and system-signature identity disagree")]
    SystemIdentity,
    #[error("finalizeWithdrawalBatch is not the unique final transaction")]
    FinalizationPosition,
    #[error("final system transaction destination is not ZoneOutbox")]
    FinalizationDestination,
    #[error("finalizeWithdrawalBatch receipt is unsuccessful")]
    FinalizationSuccess,
    #[error("finalization blockNumber does not equal the Zone block")]
    FinalizationBlockNumber,
}

/// Authenticated byte surface whose canonical encoding is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DataSource {
    #[error("advanceTempo calldata")]
    AdvanceTempoCalldata,
    #[error("advanceTempo header RLP")]
    AdvanceHeaderRlp,
    #[error("ordinary depositData")]
    OrdinaryDepositData,
    #[error("withdrawal bounce-back depositData")]
    WithdrawalBounceBackData,
    #[error("finalizeWithdrawalBatch calldata")]
    FinalizationCalldata,
    #[error("processWithdrawals calldata")]
    ProcessWithdrawalsCalldata,
    #[error("submitBatch calldata")]
    SubmitBatchCalldata,
    #[error("portal transaction calldata")]
    PortalTransactionCalldata,
}

impl DataSource {
    pub(crate) const fn chain(self) -> ProtocolChain {
        match self {
            Self::AdvanceTempoCalldata
            | Self::AdvanceHeaderRlp
            | Self::OrdinaryDepositData
            | Self::WithdrawalBounceBackData
            | Self::FinalizationCalldata => ProtocolChain::ZoneL2,
            Self::ProcessWithdrawalsCalldata
            | Self::SubmitBatchCalldata
            | Self::PortalTransactionCalldata => ProtocolChain::TempoL1,
        }
    }
}

/// Top-level Portal call family implied by authenticated receipt outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PortalCallFamily {
    #[error("submitBatch")]
    SubmitBatch,
    #[error("processWithdrawals")]
    ProcessWithdrawals,
}

/// Reconciliation failures between Portal events and top-level calldata.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PortalCallError {
    #[error(
        "unsupported nested or ambiguous portal call in transaction {transaction_hash}: target {target:?}"
    )]
    UnsupportedNestedPortalCall {
        transaction_hash: B256,
        target: Option<Address>,
    },
    #[error(
        "portal calldata/event mismatch in transaction {transaction_hash}: expected {expected}, got {actual}"
    )]
    FamilyMismatch {
        transaction_hash: B256,
        expected: PortalCallFamily,
        actual: PortalCallFamily,
    },
    #[error(
        "processWithdrawals transaction {transaction_hash} emitted processing outcomes with an empty withdrawal array"
    )]
    EmptyProcessWithOutcomes { transaction_hash: B256 },
}

/// Deterministic failure after canonical data has crossed the adapter boundary.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ObservationError {
    #[error(transparent)]
    Acquisition(#[from] AcquisitionError),
    #[error("invalid protocol envelope {location}: {rule}")]
    InvalidEnvelope {
        location: EnvelopeLocation,
        rule: EnvelopeRule,
    },
    #[error("malformed authenticated {kind} in {transaction}: {detail}")]
    MalformedAuthenticatedData {
        kind: DataSource,
        transaction: AuthenticatedTransaction,
        evidence: AuthenticatedDataEvidence,
        detail: String,
    },
    #[error(
        "{chain} protocol-event failure at transaction {transaction_index} ({transaction_hash}), receipt log {receipt_log_index}, block log {block_log_index}: {error}"
    )]
    ProtocolEvent {
        chain: ProtocolChain,
        transaction_index: usize,
        receipt_log_index: usize,
        block_log_index: usize,
        transaction_hash: B256,
        #[source]
        error: Box<ProtocolEventError>,
    },
    #[error(transparent)]
    PortalCall(#[from] PortalCallError),
}

impl ObservationError {
    pub(crate) fn malformed(
        kind: DataSource,
        transaction: AuthenticatedTransaction,
        evidence: AuthenticatedDataEvidence,
        detail: impl fmt::Display,
    ) -> Self {
        debug_assert_eq!(kind.chain(), transaction.chain());
        Self::MalformedAuthenticatedData {
            kind,
            transaction,
            evidence,
            detail: detail.to_string(),
        }
    }

    pub(crate) const fn invalid_envelope(transaction_index: usize, rule: EnvelopeRule) -> Self {
        Self::InvalidEnvelope {
            location: EnvelopeLocation::Transaction(transaction_index),
            rule,
        }
    }

    pub(crate) const fn invalid_block_envelope(rule: EnvelopeRule) -> Self {
        Self::InvalidEnvelope {
            location: EnvelopeLocation::Block,
            rule,
        }
    }

    pub(crate) fn protocol_event(
        chain: ProtocolChain,
        transaction_index: usize,
        receipt_log_index: usize,
        block_log_index: usize,
        transaction_hash: B256,
        error: ProtocolEventError,
    ) -> Self {
        Self::ProtocolEvent {
            chain,
            transaction_index,
            receipt_log_index,
            block_log_index,
            transaction_hash,
            error: Box::new(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquisition_and_protocol_failures_remain_distinct() {
        let error = AcquisitionError::missing(AcquisitionSource::L1Block, "0x01");
        let observation: ObservationError = error.into();
        assert!(matches!(
            observation,
            ObservationError::Acquisition(AcquisitionError::Missing { .. })
        ));
        assert!(matches!(
            ObservationError::invalid_envelope(0, EnvelopeRule::AdvanceSystemCaller),
            ObservationError::InvalidEnvelope { .. }
        ));
    }
}
