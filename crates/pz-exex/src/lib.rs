//! Privacy Zone ExEx - Execution Extension for Tempo Privacy Zones (L2 Validium)
//!
//! Simple in-memory state management for zone execution.
//!
//! Key components:
//! - `state`: In-memory zone state using revm's CacheDB
//! - `deposit`: Deposit processing with TIP-20 balance crediting and calldata execution
//! - `execution`: Transaction execution using tempo-evm
//! - `exex`: ExEx event loop processing L1 deposits and batches
//! - `types`: Domain types with cursor tracking and journal hashing

pub mod deposit;
pub mod error;
pub mod execution;
pub mod exex;
pub mod state;
pub mod types;

pub use deposit::{process_deposit, DepositResult};
pub use error::PzError;
pub use execution::execute_transactions;
pub use exex::{install_pz_exex, PrivacyZoneExEx};
pub use state::ZoneState;
pub use types::{
    Deposit, ExitIntent, L1Cursor, PendingTx, PortalEvent, PortalEventKind, PzConfig, PzState,
    EXIT_PRECOMPILE,
};
