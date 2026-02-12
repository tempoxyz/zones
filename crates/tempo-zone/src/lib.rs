//! Tempo Zone Node - a lightweight L2 node built on reth.
//!
//! This crate provides the node configuration and components for running a Tempo Zone L2.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(unnameable_types)]

use eyre as _;

pub mod abi;
pub mod batch;
pub mod bindings;
mod builder;
pub mod engine;
pub mod evm;
mod executor;
pub mod l1;
pub mod l1_state;
mod node;
pub mod system_tx;
pub mod withdrawals;
pub mod zonemonitor;

pub use batch::{BatchData, BatchSubmitter, BatchSubmitterConfig};
pub use engine::ZoneEngine;
pub use l1::{
    Deposit, DepositQueue, DepositQueueTransition, L1BlockDeposits, L1Subscriber,
    L1SubscriberConfig, PendingDeposits,
};
pub use node::{ZoneExecutorBuilder, ZoneNode};
pub use withdrawals::{SharedWithdrawalStore, WithdrawalProcessorConfig, WithdrawalStore};
pub use zonemonitor::{ZoneMonitorConfig, spawn_zone_monitor};

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
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
/// The monitor and withdrawal processor use the sequencer signer for L1 transactions.
///
/// Returns a [`ZoneSequencerHandle`] with join handles for both tasks.
pub fn spawn_zone_sequencer(
    config: ZoneSequencerConfig,
    signer: PrivateKeySigner,
) -> ZoneSequencerHandle {
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
        portal_address: config.portal_address,
        l1_rpc_url: config.l1_rpc_url,
        signer: signer.clone(),
    };

    let withdrawal_handle = withdrawals::spawn_withdrawal_processor(
        withdrawal_config,
        signer,
        withdrawal_store.clone(),
        withdrawal_notify.clone(),
    );
    let monitor_handle = spawn_zone_monitor(monitor_config, withdrawal_store, withdrawal_notify);

    ZoneSequencerHandle {
        withdrawal_handle,
        monitor_handle,
    }
}
