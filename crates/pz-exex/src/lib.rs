//! Privacy Zone ExEx - Execution Extension for Tempo Privacy Zones (L2 Validium)
//!
//! This crate implements a privacy zone as an ExEx attached to a Tempo L1 node.
//! Each zone:
//! - Has exactly one permissioned sequencer
//! - Bridges exactly one TIP-20 token (the zone gas token)
//! - Uses SQL database for state (no reth db, no txpool)
//! - Settles via validity proofs or TEE attestations

pub mod db;
pub mod error;
pub mod exex;
pub mod types;

pub use db::Database;
pub use error::PzError;
pub use exex::PrivacyZoneExEx;
