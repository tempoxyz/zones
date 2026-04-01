//! Configuration for the private zone RPC server.

use alloy_primitives::Address;
use std::{net::SocketAddr, time::Duration};

use crate::auth::DEFAULT_MAX_AUTH_TOKEN_VALIDITY;

/// Configuration for the private zone RPC server.
#[derive(Debug, Clone)]
pub struct PrivateRpcConfig {
    /// Address to listen on for the private RPC server.
    pub listen_addr: SocketAddr,
    /// Tempo L1 RPC URL used by zone-specific RPC methods that inspect portal logs.
    pub l1_rpc_url: String,
    /// Zone L2 RPC URL used by zone-specific RPC methods that inspect L2 events.
    pub zone_rpc_url: String,
    /// Interval between WebSocket reconnection attempts for long-lived RPC clients.
    pub retry_connection_interval: Duration,
    /// The zone's numeric identifier.
    pub zone_id: u32,
    /// The zone's chain ID.
    pub chain_id: u64,
    /// Maximum authorization token validity window this server accepts.
    ///
    /// This may be configured lower than the protocol default to tighten local
    /// policy, but it must not exceed the protocol maximum.
    pub max_auth_token_validity: Duration,
    /// The ZonePortal contract address on L1 (used for querying deposits, not for auth tokens).
    pub zone_portal: Address,
    /// The sequencer address — callers matching this get unredacted responses.
    pub sequencer: Address,
}

impl PrivateRpcConfig {
    /// Validate the private RPC configuration before starting the server.
    pub fn validate(&self) -> eyre::Result<()> {
        if self.max_auth_token_validity > DEFAULT_MAX_AUTH_TOKEN_VALIDITY {
            eyre::bail!(
                "private RPC max auth token validity ({}s) exceeds protocol maximum ({}s)",
                self.max_auth_token_validity.as_secs(),
                DEFAULT_MAX_AUTH_TOKEN_VALIDITY.as_secs(),
            );
        }

        Ok(())
    }
}
