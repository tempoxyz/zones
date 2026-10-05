//! Service-side protocol building blocks for instant Zone transfers.
//!
//! This crate intentionally does not own consensus, token execution, networking, or
//! databases. It supplies deterministic validation and state transitions for node
//! integrations, plus persistence contracts whose implementations must durably commit
//! before acknowledging peer work.

pub mod admission;
pub mod certificate;
pub mod delivery;
pub mod journal;
pub mod replenishment;
pub mod state;

pub use admission::{AdmissionController, AdmissionDecision, AdmissionError, ProtocolLimits};
pub use certificate::{CertificateError, EpochRoster, QuorumVerifier, SigningDomain};
pub use delivery::{DeliveryError, DeliveryRecord, DeliveryStore, DeliveryTransition};
pub use journal::{
    DurableJournal, JournalError, ReplicatedBlockInput, SigningRecord, Tombstone, TombstoneOutcome,
};
pub use replenishment::{ReplenishmentError, ReplenishmentJob, ReplenishmentStage};
pub use state::{DestinationRecord, SourceRecord, StateError};
