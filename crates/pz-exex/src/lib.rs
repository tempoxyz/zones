//! Privacy Zone L2 - A simple L2 execution extension.
//!
//! This follows the Signet pattern: listen to L1 ExEx notifications,
//! extract deposits, execute blocks with full EVM, persist to reth DB.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![deny(unused_must_use, rust_2018_idioms)]

mod builder;
mod node;
mod processor;
mod types;

pub use builder::PzNodeBuilder;
pub use node::PzNode;
pub use processor::PzBlockProcessor;
pub use types::{PzNodeTypes, PzNodeTypesDb};
