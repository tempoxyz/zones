//! Tempo Zone ExEx - Execution Extension for SP1 proof generation and L1 submission.
//!
//! This crate provides an ExEx that:
//! - Subscribes to zone chain state notifications
//! - Batches blocks for efficient proving
//! - Generates SP1 proofs for state transitions
//! - Submits proofs to the ZonePortal contract on L1 (Tempo)
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │                     Zone Node                           │
//! │  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐ │
//! │  │   Batcher   │───▶│   Prover    │───▶│  Submitter  │ │
//! │  └─────────────┘    └─────────────┘    └─────────────┘ │
//! │         ▲                                     │        │
//! │         │                                     ▼        │
//! │  ┌─────────────┐                      ┌─────────────┐ │
//! │  │ Chain State │                      │  ZonePortal │ │
//! │  │Notifications│                      │    (L1)     │ │
//! │  └─────────────┘                      └─────────────┘ │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use tempo_zone_exex::{install_exex, ZoneProverConfig};
//!
//! // In your node builder
//! builder.install_exex("zone-prover", |ctx| async move {
//!     Ok(install_exex(ctx, ZoneProverConfig::default()))
//! });
//! ```

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg), allow(unexpected_cfgs))]

pub mod batcher;
pub mod deposit_tracker;
pub mod events;
pub mod exex;
pub mod prover;
pub mod submitter;
pub mod types;

pub use batcher::{BatchBlockRange, BatchConfig, BatchCoordinator, BatchId};
pub use deposit_tracker::{DepositTracker, compute_deposit_hash};
pub use exex::{L1Deposit, L1DepositReceiver, ZoneProverConfig, ZoneProverExEx};
pub use prover::{MockProver, Prover, Sp1Prover, Sp1ProverConfig};
pub use submitter::{L1Submitter, SubmitterConfig};
pub use types::{
    BatchBlock, BatchCommitment, BatchInput, Deposit, IZonePortal, ProofBundle, PublicValues,
    SolBatchCommitment, SolWithdrawal, StateTransitionWitness, Withdrawal,
};

use reth_node_api::FullNodeComponents;

/// Installs the zone prover ExEx on the node.
///
/// This function creates and returns the ExEx future that should be passed
/// to the node builder's `install_exex` method.
///
/// # Example
///
/// ```ignore
/// use tempo_zone_exex::{install_exex, ZoneProverConfig};
/// use reth_node_builder::NodeBuilder;
///
/// let config = ZoneProverConfig::default();
///
/// node_builder.install_exex("zone-prover", |ctx| async move {
///     Ok(install_exex(ctx, config, None))
/// });
/// ```
pub async fn install_exex<N: FullNodeComponents>(
    ctx: reth_exex::ExExContext<N>,
    config: ZoneProverConfig,
    deposit_rx: Option<L1DepositReceiver>,
) -> eyre::Result<()> {
    let exex = ZoneProverExEx::with_deposit_receiver(ctx, config, deposit_rx).await?;
    exex.run().await
}
