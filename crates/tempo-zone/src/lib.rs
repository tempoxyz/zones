//! Tempo Zone Node - a lightweight L2 node built on reth.
//!
//! This crate provides the node configuration and components for running a Tempo Zone L2.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(unnameable_types)]
#![allow(clippy::too_many_arguments)]

use eyre as _;

pub mod abi;
pub mod batch;
pub mod bindings;
pub mod builder;
pub mod engine;
pub mod evm;
mod executor;
pub mod l1;
pub mod l1_state;
mod node;
pub mod pool;
pub mod precompiles;
pub mod rpc;
pub mod withdrawals;
pub mod zonemonitor;

pub use batch::{BatchData, BatchSubmitter};
pub use engine::ZoneEngine;
pub use l1::{
    Deposit, DepositQueue, DepositQueueTransition, EncryptedDeposit, EnqueueOutcome,
    L1BlockDeposits, L1Deposit, L1Subscriber, L1SubscriberConfig, PendingDeposits,
};
pub use l1_state::SharedL1StateCache;
pub use node::{ZoneExecutorBuilder, ZoneNode};
pub use pool::{ZoneTransactionPolicy, ZoneTransactionPool};
pub use withdrawals::{SharedWithdrawalStore, WithdrawalProcessorConfig, WithdrawalStore};
pub use zonemonitor::{ZoneMonitorConfig, spawn_zone_monitor};

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use tempo_alloy::TempoNetwork;
use tokio::sync::Notify;

/// Configuration for all zone sequencer background tasks.
#[derive(Debug, Clone)]
pub struct ZoneSequencerConfig {
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
    /// Tempo L1 RPC URL (HTTP).
    pub l1_rpc_url: String,
    /// How often the withdrawal processor polls the L1 queue.
    pub withdrawal_poll_interval: Duration,
    /// ZoneOutbox contract address on Zone L2.
    pub outbox_address: Address,
    /// ZoneInbox contract address on Zone L2.
    pub inbox_address: Address,
    /// TempoState predeploy address on Zone L2.
    pub tempo_state_address: Address,
    /// Zone L2 RPC URL (HTTP).
    pub zone_rpc_url: String,
    /// How often the zone monitor polls for new L2 blocks.
    pub zone_poll_interval: Duration,
    /// Maximum time to accumulate zone blocks before submitting a batch to L1.
    pub batch_interval: Duration,
}

/// Handles returned by [`spawn_zone_sequencer`] for managing background tasks.
pub struct ZoneSequencerHandle {
    /// Join handle for the withdrawal processor task.
    pub withdrawal_handle: tokio::task::JoinHandle<()>,
    /// Join handle for the zone monitor task (which also handles batch submission).
    pub monitor_handle: tokio::task::JoinHandle<()>,
}

/// Spawn all zone sequencer background tasks.
///
/// This is the top-level POC entrypoint that starts:
/// - **Zone monitor** — polls the Zone L2 for new blocks, extracts withdrawal events into the
///   shared store, builds [`BatchData`], and submits each batch **synchronously** to the
///   ZonePortal on Tempo L1 (with empty proof bytes). Local state only advances on
///   successful submission.
/// - **Withdrawal processor** — polls the ZonePortal withdrawal queue on Tempo L1 and calls
///   `processWithdrawal` for each pending withdrawal.
///
/// Both tasks share a **single L1 provider** (and therefore a single nonce manager)
/// to prevent signing/nonce contention when submitting concurrent L1 transactions.
///
/// Returns a [`ZoneSequencerHandle`] with join handles for both tasks.
pub async fn spawn_zone_sequencer(
    config: ZoneSequencerConfig,
    signer: PrivateKeySigner,
) -> ZoneSequencerHandle {
    // Build a single shared L1 provider with the sequencer wallet.
    // Both the batch submitter (inside the zone monitor) and the withdrawal
    // processor use this provider, ensuring nonces are tracked in one place.
    let wallet = alloy_network::EthereumWallet::from(signer);
    // FIXME: dyn provider, check if not needed
    let l1_provider: DynProvider<TempoNetwork> =
        ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(wallet)
            .connect(&config.l1_rpc_url)
            .await
            .expect("valid L1 RPC URL")
            .erased();

    let withdrawal_store: SharedWithdrawalStore = Default::default();
    let withdrawal_notify = Arc::new(Notify::new());

    let withdrawal_config = WithdrawalProcessorConfig {
        portal_address: config.portal_address,
        l1_rpc_url: config.l1_rpc_url.clone(),
        fallback_poll_interval: config.withdrawal_poll_interval,
    };

    let monitor_config = ZoneMonitorConfig {
        outbox_address: config.outbox_address,
        inbox_address: config.inbox_address,
        tempo_state_address: config.tempo_state_address,
        zone_rpc_url: config.zone_rpc_url,
        poll_interval: config.zone_poll_interval,
        batch_interval: config.batch_interval,
        portal_address: config.portal_address,
    };

    let withdrawal_handle = withdrawals::spawn_withdrawal_processor(
        withdrawal_config,
        l1_provider.clone(),
        withdrawal_store.clone(),
        withdrawal_notify.clone(),
    );
    let monitor_handle = spawn_zone_monitor(
        monitor_config,
        l1_provider,
        withdrawal_store,
        withdrawal_notify,
    );

    ZoneSequencerHandle {
        withdrawal_handle,
        monitor_handle,
    }
}
