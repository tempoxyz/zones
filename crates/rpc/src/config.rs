//! Configuration for the private zone RPC server.

use alloy_primitives::Address;
use std::{net::SocketAddr, time::Duration};

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
    /// The ZonePortal contract address on L1 (used for querying deposits, not for auth tokens).
    pub zone_portal: Address,
    /// The sequencer address — callers matching this get unredacted responses.
    pub sequencer: Address,
}
