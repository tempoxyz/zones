//! Zone-specific payload types.
//!
//! Owns the full payload attribute types for the zone, wrapping
//! [`EthPayloadBuilderAttributes`] directly and adding L1 block data plus the
//! millisecond timestamp portion. This avoids pulling in Tempo-specific
//! concepts the zone doesn't use (interrupts, subblocks, DKG extra-data).

use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes};
use alloy_rpc_types_engine::PayloadAttributes as EthPayloadAttributes;
use alloy_rpc_types_eth::Withdrawal;
use reth_node_api::{PayloadBuilderAttributes, PayloadTypes};
use reth_payload_builder::{EthBuiltPayload, EthPayloadBuilderAttributes};
use reth_primitives_traits::SealedBlock;
use serde::{Deserialize, Serialize};
use tempo_payload_types::TempoExecutionData;
use tempo_primitives::{Block, TempoPrimitives};

use crate::l1::PreparedL1Block;

/// Zone RPC payload attributes — the type that flows through FCU.
///
/// Carries standard Ethereum attributes, a millisecond timestamp portion, and
/// the prepared L1 block whose deposits should be included in this zone block.
/// The L1 data is set by the [`ZoneEngine`](crate::ZoneEngine) before sending
/// FCU and is skipped during (de)serialisation since it only travels through
/// in-process channels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZonePayloadAttributes {
    /// Standard Ethereum payload attributes.
    pub inner: EthPayloadAttributes,

    /// Milliseconds portion of the timestamp (0–999).
    pub timestamp_millis_part: u64,

    /// Prepared L1 block to process in this zone block. Every zone block
    /// processes exactly one L1 block via `advanceTempo`. Decryption and
    /// TIP-403 policy checks have already been performed by the engine.
    pub l1_block: PreparedL1Block,
}

impl reth_node_api::PayloadAttributes for ZonePayloadAttributes {
    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.inner.withdrawals()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root()
    }
}

/// Zone builder attributes — wraps [`EthPayloadBuilderAttributes`] with L1
/// data and the millisecond timestamp portion.
///
/// The `l1_block` is mandatory: every zone block processes exactly one L1
/// block via `advanceTempo`. Decryption and TIP-403 policy checks have
/// already been performed by the engine.
#[derive(Debug, Clone)]
pub struct ZonePayloadBuilderAttributes {
    inner: EthPayloadBuilderAttributes,
    timestamp_millis_part: u64,
    l1_block: PreparedL1Block,
}

impl ZonePayloadBuilderAttributes {
    /// Returns a reference to the prepared L1 block data.
    pub fn l1_block(&self) -> &PreparedL1Block {
        &self.l1_block
    }

    /// Returns the extra data for the block header (always empty for zones).
    pub fn extra_data(&self) -> Bytes {
        Bytes::default()
    }

    /// Returns the milliseconds portion of the timestamp.
    pub fn timestamp_millis_part(&self) -> u64 {
        self.timestamp_millis_part
    }
}

impl PayloadBuilderAttributes for ZonePayloadBuilderAttributes {
    type RpcPayloadAttributes = ZonePayloadAttributes;
    type Error = std::convert::Infallible;

    fn try_new(
        parent: B256,
        rpc_payload_attributes: Self::RpcPayloadAttributes,
        version: u8,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            inner: EthPayloadBuilderAttributes::try_new(
                parent,
                rpc_payload_attributes.inner,
                version,
            )?,
            timestamp_millis_part: rpc_payload_attributes.timestamp_millis_part,
            l1_block: rpc_payload_attributes.l1_block,
        })
    }

    fn payload_id(&self) -> alloy_rpc_types_engine::PayloadId {
        self.inner.payload_id()
    }

    fn parent(&self) -> B256 {
        self.inner.parent()
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root()
    }

    fn suggested_fee_recipient(&self) -> Address {
        self.inner.suggested_fee_recipient()
    }

    fn prev_randao(&self) -> B256 {
        self.inner.prev_randao()
    }

    fn withdrawals(&self) -> &alloy_rpc_types_eth::Withdrawals {
        self.inner.withdrawals()
    }
}

/// Zone payload types.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct ZonePayloadTypes;

impl PayloadTypes for ZonePayloadTypes {
    type ExecutionData = TempoExecutionData;
    type BuiltPayload = EthBuiltPayload<TempoPrimitives>;
    type PayloadAttributes = ZonePayloadAttributes;
    type PayloadBuilderAttributes = ZonePayloadBuilderAttributes;

    fn block_to_payload(block: SealedBlock<Block>) -> Self::ExecutionData {
        TempoExecutionData {
            block: Arc::new(block),
            validator_set: None,
        }
    }
}
