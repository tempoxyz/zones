//! Zone payload types.
//!
//! Owns the full payload attribute types for the zone, wrapping Ethereum
//! payload attributes and adding L1 block data plus the millisecond timestamp
//! portion. This avoids pulling in Tempo-specific concepts the zone doesn't
//! use (interrupts, subblocks, DKG extra-data).

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rpc_types_engine::{PayloadAttributes as EthPayloadAttributes, PayloadId};
use alloy_rpc_types_eth::Withdrawal;
use reth_node_api::{
    InvalidPayloadAttributesError, NewPayloadError, PayloadTypes, PayloadValidator,
};
use reth_payload_primitives::PayloadAttributes;
use reth_primitives_traits::{AlloyBlockHeader, SealedBlock, SealedHeader};
use serde::{Deserialize, Serialize};
use tempo_node::engine::TempoEngineValidator;
use tempo_payload_types::{TempoBuiltPayload, TempoExecutionData};
use tempo_primitives::{Block, TempoHeader};
use zone_evm::same_anchor::SameAnchorOpening;
use zone_l1::PreparedL1Block;

/// Encoding version of the canonical T14 fast-settlement boundary marker.
pub const FAST_SETTLEMENT_BOUNDARY_VERSION: u16 = 1;

/// Consensus reason the canonical fast-settlement boundary was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FastSettlementTrigger {
    /// The retained batch reached the 500 ms deadline first.
    Elapsed,
    /// The retained replay/proof input reached one MiB first.
    RetainedBytes,
    /// Both limits were first reached at the same canonical millisecond.
    Both,
}

/// Versioned canonical metadata for one T14 `BatchFinalized` boundary.
///
/// This record is carried in the complete payload attributes replicated by OpenRaft. The actual
/// `finalizeWithdrawalBatch` transaction carrying [`Self::finalization_calldata`] is replicated in
/// the same entry. Fixed-width integer fields allow the scheduler to fill the exact retained byte
/// count after the final block and witness have been encoded without changing its encoded length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FastSettlementBoundary {
    /// Marker encoding version. Must be [`FAST_SETTLEMENT_BOUNDARY_VERSION`].
    pub version: u16,
    /// Height of the preceding canonical `BatchFinalized` block.
    pub previous_boundary_height: u64,
    /// Hash of the preceding canonical `BatchFinalized` block.
    pub previous_boundary_hash: B256,
    /// Millisecond Zone timestamp of the preceding canonical boundary.
    pub previous_boundary_timestamp_millis: u64,
    /// Height of the block containing this boundary.
    pub block_height: u64,
    /// Parent hash of the block containing this boundary.
    pub parent_hash: B256,
    /// Millisecond Zone timestamp of the block containing this boundary.
    pub timestamp_millis: u64,
    /// Exact cumulative retained encoded block/attributes/transaction/witness bytes through this
    /// proposal, starting immediately after the preceding canonical boundary.
    pub cumulative_retained_bytes: u64,
    /// Limit which admitted this proposal.
    pub trigger: FastSettlementTrigger,
    /// Complete immutable ABI calldata of the actual finalization system transaction.
    pub finalization_calldata: Bytes,
}

impl FastSettlementBoundary {
    /// Validate fields known before payload execution. Exact retained bytes and transaction bytes
    /// are validated at the replicated-input boundary after the witness is available.
    pub fn validate_payload_binding(
        &self,
        block_height: u64,
        parent_hash: B256,
        timestamp_millis: u64,
    ) -> Result<(), &'static str> {
        if self.version != FAST_SETTLEMENT_BOUNDARY_VERSION {
            return Err("unsupported fast-settlement boundary version");
        }
        if self.block_height != block_height
            || self.parent_hash != parent_hash
            || self.timestamp_millis != timestamp_millis
        {
            return Err("fast-settlement boundary does not bind the current block");
        }
        if self.previous_boundary_height >= self.block_height
            || self.previous_boundary_timestamp_millis > self.timestamp_millis
        {
            return Err("fast-settlement boundary predecessor is not before the current block");
        }
        if self.finalization_calldata.is_empty() {
            return Err("fast-settlement boundary is missing finalization calldata");
        }
        Ok(())
    }
}

/// Explicit opening Tempo system-call variant for a Zone payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TempoImport {
    Full(Box<PreparedL1Block>),
    CheckpointOnly(Vec<SealedHeader<TempoHeader>>),
    /// Execute ordinary transactions against the finalized checkpoint already in parent state.
    SameAnchor(SameAnchorOpening),
}

impl TempoImport {
    pub(crate) fn is_checkpoint_only(&self) -> bool {
        matches!(self, Self::CheckpointOnly(_))
    }

    pub(crate) fn follows_checkpoint_blocks(&self) -> bool {
        match self {
            Self::Full(prepared) => prepared.follows_checkpoint_blocks,
            Self::CheckpointOnly(_) | Self::SameAnchor(_) => false,
        }
    }

    pub(crate) fn total_deposits(&self) -> usize {
        match self {
            Self::Full(prepared) => prepared.queued_deposits.len(),
            Self::CheckpointOnly(_) | Self::SameAnchor(_) => 0,
        }
    }

    pub(crate) fn enabled_tokens(&self) -> usize {
        match self {
            Self::Full(prepared) => prepared.enabled_tokens.len(),
            Self::CheckpointOnly(_) | Self::SameAnchor(_) => 0,
        }
    }
}

/// Zone RPC payload attributes — the type that flows through FCU.
///
/// Carries standard Ethereum attributes, a millisecond timestamp portion, and
/// the prepared L1 block whose deposits should be included in this zone block.
/// The L1 data is set by the ZoneEngine before sending
/// FCU and is skipped during (de)serialisation since it only travels through
/// in-process channels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZonePayloadAttributes {
    /// Standard Ethereum payload attributes.
    pub inner: EthPayloadAttributes,

    /// Milliseconds portion of the timestamp (0–999).
    pub timestamp_millis_part: u64,

    /// Prepared L1 block to process in this zone block. Every zone block
    /// processes exactly one L1 block via `advanceTempo`. Decryption and ABI
    /// encoding have already been performed by the engine; TIP-403 policy is
    /// enforced during `advanceTempo` when the deposits mint TIP-20 tokens.
    pub tempo_import: TempoImport,

    /// Canonical T14 fast-settlement boundary selected from the fsynced OpenRaft prefix.
    #[serde(default)]
    pub fast_settlement_boundary: Option<FastSettlementBoundary>,
}

impl reth_node_api::PayloadAttributes for ZonePayloadAttributes {
    fn payload_id(&self, parent_hash: &B256) -> PayloadId {
        let legacy = reth_payload_primitives::payload_id(parent_hash, &self.inner);
        let Some(boundary) = &self.fast_settlement_boundary else {
            return legacy;
        };
        // A boundary rebuild uses the same parent and timestamp as its first-pass candidate. Mix
        // the canonical marker into the cache key so the payload service cannot return that
        // unfinalized first pass. Preserve the exact legacy ID when no marker is present.
        let mut material = Vec::with_capacity(8 + 256);
        material.extend_from_slice(legacy.0.as_slice());
        material.extend_from_slice(&boundary.version.to_be_bytes());
        material.extend_from_slice(&boundary.previous_boundary_height.to_be_bytes());
        material.extend_from_slice(boundary.previous_boundary_hash.as_slice());
        material.extend_from_slice(&boundary.previous_boundary_timestamp_millis.to_be_bytes());
        material.extend_from_slice(&boundary.block_height.to_be_bytes());
        material.extend_from_slice(boundary.parent_hash.as_slice());
        material.extend_from_slice(&boundary.timestamp_millis.to_be_bytes());
        material.extend_from_slice(&boundary.cumulative_retained_bytes.to_be_bytes());
        material.push(match boundary.trigger {
            FastSettlementTrigger::Elapsed => 0,
            FastSettlementTrigger::RetainedBytes => 1,
            FastSettlementTrigger::Both => 2,
        });
        material.extend_from_slice(boundary.finalization_calldata.as_ref());
        let digest = keccak256(material);
        PayloadId::new(
            digest[..8]
                .try_into()
                .expect("keccak digest has eight bytes"),
        )
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.inner.withdrawals.as_ref()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root
    }

    fn slot_number(&self) -> Option<u64> {
        self.inner.slot_number
    }
}

impl ZonePayloadAttributes {
    /// Returns a reference to the prepared L1 block data.
    pub fn tempo_import(&self) -> &TempoImport {
        &self.tempo_import
    }

    /// Returns the canonical fast-settlement boundary, when this block closes one.
    pub fn fast_settlement_boundary(&self) -> Option<&FastSettlementBoundary> {
        self.fast_settlement_boundary.as_ref()
    }

    /// Returns the extra data for the block header (always empty for zones).
    pub fn extra_data(&self) -> Bytes {
        Bytes::default()
    }

    /// Returns the milliseconds portion of the timestamp.
    pub fn timestamp_millis_part(&self) -> u64 {
        self.timestamp_millis_part
    }

    pub fn suggested_fee_recipient(&self) -> Address {
        self.inner.suggested_fee_recipient
    }

    pub fn prev_randao(&self) -> B256 {
        self.inner.prev_randao
    }
}

/// Zone payload types.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct ZonePayloadTypes;

impl PayloadTypes for ZonePayloadTypes {
    type ExecutionData = TempoExecutionData;
    type BuiltPayload = TempoBuiltPayload;
    type PayloadAttributes = ZonePayloadAttributes;

    fn block_to_payload(
        block: SealedBlock<Block>,
        bal: Option<alloy_primitives::Bytes>,
    ) -> Self::ExecutionData {
        TempoExecutionData {
            block: block.into(),
            block_access_list: bal,
        }
    }
}

impl PayloadValidator<ZonePayloadTypes> for TempoEngineValidator {
    type Block = Block;

    fn convert_payload_to_block(
        &self,
        payload: TempoExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        let TempoExecutionData {
            block,
            block_access_list: _,
        } = payload;
        Ok(block.into_sealed_block())
    }

    fn validate_payload_attributes_against_header(
        &self,
        attr: &ZonePayloadAttributes,
        header: &TempoHeader,
    ) -> Result<(), InvalidPayloadAttributesError> {
        if PayloadAttributes::timestamp(attr) < AlloyBlockHeader::timestamp(header) {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }
        Ok(())
    }
}
