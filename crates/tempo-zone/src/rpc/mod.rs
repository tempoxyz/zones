//! Private zone RPC server.
//!
//! Provides an authenticated JSON-RPC endpoint that sits in front of the
//! standard reth RPC, adding per-caller privacy redactions and access control.

pub mod api;
pub mod auth;
pub mod config;
pub mod filter;
pub mod handlers;
pub mod provider;
pub mod server;
pub mod types;

pub use api::TempoZoneRpc;
pub use config::PrivateRpcConfig;
pub use handlers::ZoneRpcApi;
pub use provider::{ZoneProvider, ZoneProviderConfig};
pub use server::start_private_rpc;
