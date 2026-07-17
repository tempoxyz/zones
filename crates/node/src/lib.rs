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
mod fee_token_validator;
pub mod genesis;
pub mod node;
mod replication;
pub mod rpc;

pub use engine::ZoneEngine;
pub use node::{ZoneExecutorBuilder, ZoneNode, ZonePrivateRpcConfig, ZoneSequencerAddOnsConfig};
