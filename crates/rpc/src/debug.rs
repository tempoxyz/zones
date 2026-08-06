//! Zone-specific debug RPC extensions.

use alloy_rpc_types_eth::BlockNumberOrTag;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use crate::types::ZoneExecutionWitness;

/// Debug methods that extend reth's standard execution-witness API for Zones.
#[rpc(server, namespace = "debug")]
pub trait ZoneDebugApi {
    /// Replays a Zone block and returns its execution witness and Tempo L1 storage reads.
    #[method(name = "zoneExecutionWitness")]
    async fn zone_execution_witness(
        &self,
        block: BlockNumberOrTag,
    ) -> RpcResult<ZoneExecutionWitness>;
}
