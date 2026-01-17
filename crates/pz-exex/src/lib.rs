//! Privacy Zone L2 - A simple L2 execution extension.
//!
//! This follows the Signet pattern: listen to L1 ExEx notifications,
//! extract deposits, execute blocks with full EVM, persist to reth DB.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![deny(unused_must_use, rust_2018_idioms)]

use reth_evm as _;
use reth_tracing as _;
use tempo_precompiles as _;
use tempo_primitives as _;
use tokio as _;

mod builder;
pub mod db;
pub mod execution;
mod node;
pub mod portal;
mod processor;
mod types;

pub use builder::{NoDb, PzNodeBuilder};
pub use db::L2Database;
pub use execution::{execute_block, process_deposit};
pub use node::PzNode;
pub use processor::{PzBlockProcessor, ReceiptExt};
pub use types::{PzNodeTypes, PzNodeTypesDb};

// TODO: implement rpc and then we can strip it out further so that its a limited RPC
