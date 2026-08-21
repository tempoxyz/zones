//! Configuration for canonical Zone observation and persistent batch submission.

use std::time::Duration;

use alloy_primitives::Address;

use crate::{AttestationStore, settlement::BatchAnchorConfig};

/// Configuration shared by the generation-scoped candidate monitor and persistent actor backend.
#[derive(Debug, Clone)]
pub struct ZoneMonitorConfig {
    /// ZoneOutbox contract address on Zone L2.
    pub outbox_address: Address,
    /// ZoneInbox contract address on Zone L2.
    pub inbox_address: Address,
    /// Fallback interval for reconciling the canonical Zone head.
    pub poll_interval: Duration,
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
    /// EIP-2935 history and safety-margin limits used by the batch submitter.
    pub batch_anchor_config: BatchAnchorConfig,
    /// Shared P2P attestations, required after a settlement signer set is activated.
    pub attestation_store: Option<AttestationStore>,
}
