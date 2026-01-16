//! Privacy Zone ExEx - Execution Extension for Tempo Privacy Zones (L2 Validium)
//!
//! A simple ExEx with SQL database, based on reth-exex-examples/rollup pattern.
//!
//! Key components:
//! - `db`: SQL-backed state with `reth_revm::Database` implementation
//! - `execution`: Transaction execution using tempo-evm
//! - `exex`: ExEx event loop processing L1 deposits and batches

pub mod db;
pub mod error;
pub mod execution;
pub mod exex;
pub mod types;

pub use db::Database;
pub use error::{PzDbError, PzError};
pub use execution::execute_transactions;
pub use exex::{install_pz_exex, PrivacyZoneExEx};
pub use types::{Deposit, ExitIntent, PzConfig, PzState};
