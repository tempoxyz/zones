//! Service-side protocol building blocks for instant Zone transfers.
//!
//! This crate intentionally does not own consensus, token execution, networking, or
//! databases. It supplies deterministic validation and state transitions for node
//! integrations, plus persistence contracts whose implementations must durably commit
//! before acknowledging peer work.

pub mod admission;
pub mod certificate;
pub mod delivery;
pub mod drain;
pub mod journal;
mod journal_codec;
pub mod replenishment;
pub mod replenishment_worker;
pub mod state;
pub mod transport;

pub use admission::{AdmissionController, AdmissionDecision, AdmissionError, ProtocolLimits};
pub use certificate::{CertificateError, EpochRoster, QuorumVerifier, SigningDomain};
pub use delivery::{DeliveryError, DeliveryRecord, DeliveryStore, DeliveryTransition};
pub use journal::{
    CompletedEconomicAttempt, DurableJournal, EconomicActionKind, EconomicActionRecord,
    EconomicAttemptOutcome, JournalError, JournalIncomingRecord, ReplicatedBlockInput,
    SigningRecord, Tombstone, TombstoneOutcome,
};
pub use replenishment::{
    DepositRecord, InventoryContribution, ReplenishmentError, ReplenishmentJob, ReplenishmentStage,
    TokenAmount, TransactionCost, WithdrawalRecord,
};
pub use replenishment_worker::{
    LegObservation, ReplenishmentBridge, ReplenishmentStore, ReplenishmentWorker,
    ReplenishmentWorkerError, WorkerFuture, WorkerProgress,
};
pub use state::{DestinationRecord, SourceRecord, StateError};
pub use transport::{AuthenticatedPeerSession, TransportError};
