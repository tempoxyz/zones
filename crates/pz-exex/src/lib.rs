//! Privacy Zone ExEx - Execution Extension for Tempo Privacy Zones (L2 Validium)
//!
//! A simple ExEx with SQL database, based on reth-exex-examples/rollup pattern
//! with Signet-inspired improvements for event extraction and reorg handling.
//!
//! Key components:
//! - `db`: SQL-backed state with `reth_revm::Database` implementation
//! - `block_builder`: Zone block construction from deposits and transactions
//! - `execution`: Transaction execution using tempo-evm
//! - `exex`: ExEx event loop processing L1 deposits and batches
//! - `types`: Domain types with cursor tracking and journal hashing

pub mod block_builder;
pub mod db;
pub mod error;
pub mod execution;
pub mod exex;
pub mod types;

pub use block_builder::{ZoneBlock, ZoneBlockBuilder};
pub use db::Database;
pub use error::{PzDbError, PzError};
pub use execution::execute_transactions;
pub use exex::{install_pz_exex, PrivacyZoneExEx};
pub use types::{
    Deposit, ExitIntent, L1Cursor, PortalEvent, PortalEventKind, PzConfig, PzState,
    EXIT_PRECOMPILE,
};
