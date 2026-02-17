//! [`ZoneRpcApi`] implementation backed by reth's EthApi.

use std::future::Future;
use std::pin::Pin;

use alloy_network::{ReceiptResponse, TransactionResponse};
use alloy_primitives::{Bloom, B256};
use alloy_rpc_types_eth::{BlockNumberOrTag, BlockTransactions};
use reth_rpc_eth_api::{
    helpers::{EthBlocks, EthTransactions, FullEthApi},
    EthApiTypes,
};
use serde_json::Value;
use tempo_alloy::TempoNetwork;

use super::auth::AuthContext;
use super::handlers::ZoneRpcApi;

/// [`ZoneRpcApi`] implementation backed by reth's EthApi.
///
/// Performs typed privacy redactions (e.g. zeroing `logsBloom`, clearing
/// transactions, filtering by sender) *before* serializing to JSON.
pub struct TempoZoneRpc<Api> {
    api: Api,
}

impl<Api> TempoZoneRpc<Api> {
    /// Wrap an EthApi instance.
    pub fn new(api: Api) -> Self {
        Self { api }
    }
}

impl<Api> ZoneRpcApi for TempoZoneRpc<Api>
where
    Api: FullEthApi + EthApiTypes<NetworkTypes = TempoNetwork> + Send + Sync + 'static,
{
    fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
        auth: AuthContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + '_>> {
        Box::pin(async move {
            let block = EthBlocks::rpc_block(&self.api, number.into(), full)
                .await
                .map_err(|e| e.to_string())?;

            let Some(mut block) = block else { return Ok(Value::Null) };

            if !auth.is_sequencer {
                block.header.inner.inner.inner.logs_bloom = Bloom::ZERO;
                block.transactions = BlockTransactions::Hashes(vec![]);
            }

            serde_json::to_value(block).map_err(|e| e.to_string())
        })
    }

    fn block_by_hash(
        &self,
        hash: B256,
        full: bool,
        auth: AuthContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + '_>> {
        Box::pin(async move {
            let block = EthBlocks::rpc_block(&self.api, hash.into(), full)
                .await
                .map_err(|e| e.to_string())?;

            let Some(mut block) = block else { return Ok(Value::Null) };

            if !auth.is_sequencer {
                block.header.inner.inner.inner.logs_bloom = Bloom::ZERO;
                block.transactions = BlockTransactions::Hashes(vec![]);
            }

            serde_json::to_value(block).map_err(|e| e.to_string())
        })
    }

    fn transaction_by_hash(
        &self,
        hash: B256,
        auth: AuthContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + '_>> {
        Box::pin(async move {
            let tx = EthTransactions::transaction_by_hash(&self.api, hash)
                .await
                .map_err(|e| e.to_string())?
                .map(|src| src.into_transaction(self.api.converter()))
                .transpose()
                .map_err(|e| e.to_string())?;

            let Some(tx) = tx else { return Ok(Value::Null) };

            if !auth.is_sequencer && tx.from() != auth.caller {
                return Ok(Value::Null);
            }

            serde_json::to_value(tx).map_err(|e| e.to_string())
        })
    }

    fn transaction_receipt(
        &self,
        hash: B256,
        auth: AuthContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + '_>> {
        Box::pin(async move {
            let receipt = EthTransactions::transaction_receipt(&self.api, hash)
                .await
                .map_err(|e| e.to_string())?;

            let Some(receipt) = receipt else { return Ok(Value::Null) };

            if !auth.is_sequencer && receipt.from() != auth.caller {
                return Ok(Value::Null);
            }

            serde_json::to_value(receipt).map_err(|e| e.to_string())
        })
    }
}
