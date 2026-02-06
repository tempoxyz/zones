//! Tempo Zone Node - a lightweight L2 node built on reth.
//!
//! This crate provides the node configuration and components for running a Tempo Zone L2.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(unnameable_types)]

use eyre as _;

pub mod l1;
mod node;

pub use l1::{Deposit, DepositQueue, L1SubscriberConfig, spawn_l1_subscriber};
pub use node::ZoneNode;
