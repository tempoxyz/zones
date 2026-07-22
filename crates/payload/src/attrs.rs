//! Zone payload types.
//!
//! Owns the full payload attribute types for the zone, wrapping Ethereum
//! payload attributes and adding L1 block data plus the millisecond timestamp
//! portion. This avoids pulling in Tempo-specific concepts the zone doesn't
//! use (interrupts, subblocks, DKG extra-data).

use alloy_consensus::Transaction;
use alloy_primitives::{Address, B256, Bytes, TxKind};
use alloy_rpc_types_engine::{PayloadAttributes as EthPayloadAttributes, PayloadId};
use alloy_rpc_types_eth::Withdrawal;
use alloy_sol_types::SolCall;
use reth_node_api::{
    InvalidPayloadAttributesError, NewPayloadError, PayloadTypes, PayloadValidator,
};
use reth_payload_primitives::PayloadAttributes;
use reth_primitives_traits::{AlloyBlockHeader, SealedBlock};
use serde::{Deserialize, Serialize};
use tempo_node::engine::TempoEngineValidator;
use tempo_payload_types::{TempoBuiltPayload, TempoExecutionData};
use tempo_primitives::{Block, TempoHeader, TempoTxEnvelope};
use tempo_zone_contracts::{ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZoneInbox, ZoneOutbox};
use zone_l1::PreparedL1Block;

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
    pub l1_block: PreparedL1Block,
}

impl reth_node_api::PayloadAttributes for ZonePayloadAttributes {
    fn payload_id(&self, parent_hash: &B256) -> PayloadId {
        reth_payload_primitives::payload_id(parent_hash, &self.inner)
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
            validator_set: None,
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
            validator_set: _,
        } = payload;
        let block = block.into_sealed_block();
        let mut transactions = block.body().transactions.iter();

        let Some(first) = transactions.next() else {
            return Err(NewPayloadError::other(reth_errors::RethError::msg(
                "zone block is missing its required advanceTempo transaction",
            )));
        };

        if !is_advance_tempo(first) {
            return Err(NewPayloadError::other(reth_errors::RethError::msg(
                "advanceTempo must be the first transaction in every zone block",
            )));
        }

        for tx in transactions {
            if is_advance_tempo(tx) {
                return Err(NewPayloadError::other(reth_errors::RethError::msg(
                    "advanceTempo must appear exactly once in every zone block",
                )));
            }

            if tx.is_system_tx() {
                if !is_finalize_withdrawal_batch(tx) {
                    return Err(NewPayloadError::other(reth_errors::RethError::msg(
                        "unrecognized zone system transaction",
                    )));
                }
            } else if is_state_changing_system_operation(tx.kind(), tx.input()) {
                return Err(NewPayloadError::other(reth_errors::RethError::msg(
                    "advanceTempo and finalizeWithdrawalBatch require a system transaction",
                )));
            }
        }

        Ok(block)
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

fn is_advance_tempo(tx: &TempoTxEnvelope) -> bool {
    tx.is_system_tx()
        && tx.kind() == TxKind::Call(ZONE_INBOX_ADDRESS)
        && tx.input().get(..4) == Some(ZoneInbox::advanceTempoCall::SELECTOR.as_slice())
}
fn is_finalize_withdrawal_batch(tx: &TempoTxEnvelope) -> bool {
    tx.is_system_tx()
        && tx.kind() == TxKind::Call(ZONE_OUTBOX_ADDRESS)
        && tx.input().get(..4) == Some(ZoneOutbox::finalizeWithdrawalBatchCall::SELECTOR.as_slice())
}

fn is_state_changing_system_operation(target: TxKind, input: &[u8]) -> bool {
    match (target, input.get(..4)) {
        (TxKind::Call(ZONE_INBOX_ADDRESS), Some(selector)) => {
            selector == ZoneInbox::advanceTempoCall::SELECTOR
        }
        (TxKind::Call(ZONE_OUTBOX_ADDRESS), Some(selector)) => {
            selector == ZoneOutbox::finalizeWithdrawalBatchCall::SELECTOR
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{Address, Signature};
    use reth_primitives_traits::SealedBlock;
    use tempo_primitives::{
        BlockBody,
        transaction::envelope::TEMPO_SYSTEM_TX_SIGNATURE,
    };

    const TOKEN: Address = Address::new([0x20; 20]);

    fn legacy_call(target: Address, input: Bytes, signature: Signature) -> TempoTxEnvelope {
        TempoTxEnvelope::Legacy(Signed::new_unhashed(
            TxLegacy {
                to: target.into(),
                input,
                ..Default::default()
            },
            signature,
        ))
    }

    fn payload_with_call(
        target: Address,
        input: Bytes,
        signature: Signature,
    ) -> TempoExecutionData {
        let advance_tempo = legacy_call(
            ZONE_INBOX_ADDRESS,
            ZoneInbox::advanceTempoCall::SELECTOR.to_vec().into(),
            TEMPO_SYSTEM_TX_SIGNATURE,
        );
        let transaction = legacy_call(target, input, signature);
        let block = Block {
            header: TempoHeader::default(),
            body: BlockBody {
                transactions: vec![advance_tempo, transaction],
                ommers: Vec::new(),
                withdrawals: None,
            },
        };

        TempoExecutionData {
            block: SealedBlock::seal_slow(block).into(),
            block_access_list: None,
            validator_set: None,
        }
    }

    fn convert_payload(payload: TempoExecutionData) -> Result<SealedBlock<Block>, NewPayloadError> {
        <TempoEngineValidator as PayloadValidator<ZonePayloadTypes>>::convert_payload_to_block(
            &TempoEngineValidator::new(),
            payload,
        )
    }

    #[test]
    fn enforces_system_and_user_signature_roles_during_payload_conversion() {
        let error = convert_payload(payload_with_call(
            TOKEN,
            Bytes::new(),
            TEMPO_SYSTEM_TX_SIGNATURE,
        ))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unrecognized zone system transaction")
        );

        let finalize: Bytes = ZoneOutbox::finalizeWithdrawalBatchCall::SELECTOR
            .to_vec()
            .into();
        assert!(
            convert_payload(payload_with_call(
                ZONE_OUTBOX_ADDRESS,
                finalize.clone(),
                TEMPO_SYSTEM_TX_SIGNATURE,
            ))
            .is_ok()
        );

        let error = convert_payload(payload_with_call(
            ZONE_OUTBOX_ADDRESS,
            finalize,
            Signature::test_signature(),
        ))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("advanceTempo and finalizeWithdrawalBatch require a system transaction")
        );
    }
}
