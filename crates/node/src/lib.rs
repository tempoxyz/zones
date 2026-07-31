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
pub mod genesis;
pub mod node;
mod replication;
pub mod role;
pub mod rpc;
mod settlement_attestation;
mod tx_forwarding;

pub use engine::{EngineExit, ProductionPermit, ZoneEngine};
pub use node::{ZoneExecutorBuilder, ZoneNode, ZonePrivateRpcConfig, ZoneSequencerAddOnsConfig};
