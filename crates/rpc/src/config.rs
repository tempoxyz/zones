//! Configuration for the private zone RPC server.

use alloy_primitives::Address;
use std::net::SocketAddr;

/// Configuration for the private zone RPC server.
#[derive(Debug, Clone)]
pub struct PrivateRpcConfig {
    /// Address to listen on for the private RPC server.
    pub listen_addr: SocketAddr,
    /// Tempo L1 RPC URL used by zone-specific RPC methods that inspect portal logs.
    pub l1_rpc_url: String,
    /// Zone L2 RPC URL used by zone-specific RPC methods that inspect L2 events.
    pub zone_rpc_url: String,
    /// The zone's numeric identifier.
    pub zone_id: u64,
    /// The zone's chain ID.
    pub chain_id: u64,
    /// The ZonePortal contract address on L1.
    pub zone_portal: Address,
    /// The sequencer address — callers matching this get unredacted responses.
    pub sequencer: Address,
}
