//! Privacy Zone ExEx - Execution Extension for Tempo Privacy Zones (L2 Validium)
//!
//! In-memory state management for zone execution with block building.
//!
//! Key components:
//! - `state`: In-memory zone state using revm's CacheDB
//! - `deposit`: Deposit processing with TIP-20 balance crediting and calldata execution
//! - `execution`: Transaction execution using tempo-evm
//! - `exex`: ExEx event loop processing L1 deposits and batches
//! - `builder`: Zone block builder running on 250ms interval
//! - `types`: Domain types with cursor tracking and journal hashing
//! - `root`: State root computation from bundle state
//! - `storage`: Persistence for zone blocks and state

pub mod builder;
pub mod deposit;
pub mod error;
pub mod execution;
pub mod exex;
pub mod root;
pub mod state;
pub mod storage;
pub mod types;

pub use builder::{SharedZoneState, ZoneBlock, ZoneBlockBuilder};
pub use deposit::{DepositResult, process_deposit};
pub use error::PzError;
pub use execution::execute_transactions;
pub use exex::{PrivacyZoneExEx, install_pz_exex};
pub use root::{compute_state_root, compute_transactions_root, merge_bundles};
pub use state::ZoneState;
pub use storage::ZoneStorage;
pub use types::{
    Deposit, EXIT_PRECOMPILE, ExitIntent, L1Cursor, PendingTx, PortalEvent, PortalEventKind,
    PzConfig, PzState,
};
