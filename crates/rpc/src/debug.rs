//! Zone-specific debug RPC extensions.

use alloy_primitives::B256;
use alloy_rpc_types_eth::BlockNumberOrTag;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use crate::types::ZoneExecutionWitness;

/// In-process Zone debug API contract.
#[jsonrpsee::core::async_trait]
pub trait ZoneDebugApi: Send + Sync {
    /// Replays a Zone block and returns its execution witness and Tempo L1 storage reads.
    async fn zone_execution_witness(
        &self,
        block: BlockNumberOrTag,
    ) -> RpcResult<ZoneExecutionWitness>;

    /// Replays an executed block by hash, including a not-yet-canonical engine payload.
    async fn zone_execution_witness_by_hash(&self, hash: B256) -> RpcResult<ZoneExecutionWitness>;
}

/// JSON-RPC transport adapter for [`ZoneDebugApi`].
#[rpc(server, namespace = "debug")]
pub trait ZoneDebugApiRpc {
    /// Replays a Zone block and returns its execution witness and Tempo L1 storage reads.
    #[method(name = "zoneExecutionWitness")]
    async fn zone_execution_witness(
        &self,
        block: BlockNumberOrTag,
    ) -> RpcResult<ZoneExecutionWitness>;
}

#[jsonrpsee::core::async_trait]
impl<T> ZoneDebugApiRpcServer for T
where
    T: ZoneDebugApi + 'static,
{
    async fn zone_execution_witness(
        &self,
        block: BlockNumberOrTag,
    ) -> RpcResult<ZoneExecutionWitness> {
        ZoneDebugApi::zone_execution_witness(self, block).await
    }
}
