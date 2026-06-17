#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(unnameable_types)]
#![allow(clippy::too_many_arguments)]
use eyre as _;

pub mod abi;
#[cfg(feature = "cli")]
pub mod cli;
pub mod ext;
pub use ext::{ChainTempoStateExt, TempoStateExt};
pub mod batch;
pub mod engine;
pub mod evm;
mod executor;
pub mod l1;
pub mod l1_state;
mod metrics;
mod node;
pub mod nonce_keys;
pub mod payload;
pub mod precompiles;
pub mod rpc;
pub mod sequencer;
mod tx_context;
pub mod withdrawals;
pub mod zonemonitor;

pub use batch::{BatchAnchorConfig, BatchData, BatchSubmitter};
pub use engine::ZoneEngine;
pub use l1::{
    Deposit, DepositQueue, EnabledToken, EncryptedDeposit, L1BlockDeposits, L1Deposit,
    L1PortalEvents, L1SequencerEvent, L1Subscriber, L1SubscriberConfig,
};
pub use l1_state::{L1StateCache, PolicyCache, PolicyProvider};
pub use node::{ZoneExecutorBuilder, ZoneNode, ZonePrivateRpcConfig, ZoneSequencerAddOnsConfig};
pub use payload::{ZonePayloadAttributes, ZonePayloadTypes};
pub use sequencer::{ZoneSequencerConfig, ZoneSequencerHandle, spawn_zone_sequencer};
pub use withdrawals::{SharedWithdrawalStore, WithdrawalProcessorConfig, WithdrawalStore};
pub use zonemonitor::{ZoneMonitorConfig, spawn_zone_monitor};
