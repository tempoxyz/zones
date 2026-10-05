#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(unnameable_types)]
#![allow(clippy::too_many_arguments)]

use eyre as _;

#[cfg(feature = "cli")]
pub mod cli;
pub mod dev;
pub mod engine;
pub mod fast_batch;
pub mod fast_drain;
pub mod fast_drain_adapters;
pub mod fast_drain_state;
pub mod fast_execution;
pub mod fast_exposure;
pub mod fast_network;
pub mod fast_quorum;
pub mod fast_raft_state_machine;
pub mod fast_raft_store;
pub mod fast_runtime;
pub mod fast_service;
pub mod fast_service_adapters;
mod follower;
pub mod genesis;
pub mod node;
mod replication;
pub mod role;
pub mod rpc;
mod settlement_attestation;
mod shadow_prover;
mod tx_forwarding;
pub mod version;

pub use engine::{EngineExit, ProductionPermit, ZoneEngine};
pub use node::{
    ZoneExecutorBuilder, ZoneNode, ZoneProverConfig, ZoneRedactedRpcConfig,
    ZoneSequencerAddOnsConfig,
};
pub use version::init_version_metadata;
