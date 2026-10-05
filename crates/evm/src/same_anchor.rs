//! Canonical state-neutral opening transaction for same-anchor blocks.

use alloy_consensus::{Signed, TxLegacy};
use alloy_primitives::{B256, Bytes, U256, keccak256};
use reth_primitives_traits::Recovered;
use tempo_primitives::{
    TempoTxEnvelope,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};

pub const SAME_ANCHOR_FORMAT_V1: u8 = 1;
pub const SAME_ANCHOR_MAX_AGE_MILLIS: u64 = 2_000;
const SAME_ANCHOR_SIGNATURE: &[u8] = b"sameAnchor(uint8,uint64,bytes32,uint64,uint64,uint16)";
const SAME_ANCHOR_CALLDATA_LEN: usize = 4 + 32 * 6;

/// Values committed by a same-anchor block's first system transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(rename_all = "camelCase", deny_unknown_fields)
)]
pub struct SameAnchorOpening {
    pub format: u8,
    pub tempo_block_number: u64,
    pub tempo_block_hash: B256,
    pub tempo_timestamp: u64,
    pub tempo_timestamp_millis_part: u16,
    pub protocol_epoch: u64,
}

impl SameAnchorOpening {
    pub const fn v1(
        tempo_block_number: u64,
        tempo_block_hash: B256,
        tempo_timestamp: u64,
        tempo_timestamp_millis_part: u16,
        protocol_epoch: u64,
    ) -> Self {
        Self {
            format: SAME_ANCHOR_FORMAT_V1,
            tempo_block_number,
            tempo_block_hash,
            tempo_timestamp,
            tempo_timestamp_millis_part,
            protocol_epoch,
        }
    }

    pub fn validate(&self) -> Result<(), SameAnchorError> {
        if self.format != SAME_ANCHOR_FORMAT_V1 {
            return Err(SameAnchorError::UnsupportedFormat(self.format));
        }
        if self.tempo_timestamp_millis_part >= 1_000 {
            return Err(SameAnchorError::InvalidAnchorMillis(
                self.tempo_timestamp_millis_part,
            ));
        }
        self.anchor_timestamp_millis()?;
        Ok(())
    }

    /// Enforce strictly increasing millisecond time and the two-second anchor bound.
    pub fn validate_block_timestamp(
        &self,
        parent_timestamp_millis: u64,
        block_timestamp: u64,
        block_timestamp_millis_part: u64,
    ) -> Result<(), SameAnchorError> {
        self.validate()?;
        if block_timestamp_millis_part >= 1_000 {
            return Err(SameAnchorError::InvalidBlockMillis(
                block_timestamp_millis_part,
            ));
        }
        let actual = block_timestamp
            .checked_mul(1_000)
            .and_then(|value| value.checked_add(block_timestamp_millis_part))
            .ok_or(SameAnchorError::TimestampOverflow)?;
        if actual <= parent_timestamp_millis {
            return Err(SameAnchorError::TimestampNotIncreasing {
                parent: parent_timestamp_millis,
                actual,
            });
        }
        let maximum = self
            .anchor_timestamp_millis()?
            .checked_add(SAME_ANCHOR_MAX_AGE_MILLIS)
            .ok_or(SameAnchorError::TimestampOverflow)?;
        if actual > maximum {
            return Err(SameAnchorError::AnchorExpired { maximum, actual });
        }
        Ok(())
    }

    pub fn encode_calldata(&self) -> Result<Bytes, SameAnchorError> {
        self.validate()?;
        let mut encoded = Vec::with_capacity(SAME_ANCHOR_CALLDATA_LEN);
        encoded.extend_from_slice(&same_anchor_selector());
        push_word(&mut encoded, U256::from(self.format));
        push_word(&mut encoded, U256::from(self.tempo_block_number));
        encoded.extend_from_slice(self.tempo_block_hash.as_slice());
        push_word(&mut encoded, U256::from(self.tempo_timestamp));
        push_word(&mut encoded, U256::from(self.protocol_epoch));
        push_word(&mut encoded, U256::from(self.tempo_timestamp_millis_part));
        Ok(encoded.into())
    }

    pub fn decode_calldata(input: &[u8]) -> Result<Self, SameAnchorError> {
        if input.len() != SAME_ANCHOR_CALLDATA_LEN || input[..4] != same_anchor_selector() {
            return Err(SameAnchorError::InvalidCalldata);
        }
        let opening = Self {
            format: decode_u64_word(&input[4..36])?
                .try_into()
                .map_err(|_| SameAnchorError::InvalidCalldata)?,
            tempo_block_number: decode_u64_word(&input[36..68])?,
            tempo_block_hash: B256::from_slice(&input[68..100]),
            tempo_timestamp: decode_u64_word(&input[100..132])?,
            protocol_epoch: decode_u64_word(&input[132..164])?,
            tempo_timestamp_millis_part: decode_u64_word(&input[164..196])?
                .try_into()
                .map_err(|_| SameAnchorError::InvalidCalldata)?,
        };
        opening.validate()?;
        Ok(opening)
    }

    /// Construct the deterministic transaction committed in both production and proof replay.
    pub fn system_transaction(
        &self,
        chain_id: u64,
    ) -> Result<Recovered<TempoTxEnvelope>, SameAnchorError> {
        let tx = TxLegacy {
            chain_id: Some(chain_id),
            nonce: 0,
            gas_price: 0,
            gas_limit: 0,
            // The pinned Tempo dependency has no native same-anchor entrypoint. This is a
            // state-neutral commitment destination; chain-spec activation remains fail-closed.
            to: TEMPO_SYSTEM_TX_SENDER.into(),
            value: U256::ZERO,
            input: self.encode_calldata()?,
        };
        Ok(Recovered::new_unchecked(
            TempoTxEnvelope::Legacy(Signed::new_unhashed(tx, TEMPO_SYSTEM_TX_SIGNATURE)),
            TEMPO_SYSTEM_TX_SENDER,
        ))
    }

    fn anchor_timestamp_millis(&self) -> Result<u64, SameAnchorError> {
        self.tempo_timestamp
            .checked_mul(1_000)
            .and_then(|value| value.checked_add(u64::from(self.tempo_timestamp_millis_part)))
            .ok_or(SameAnchorError::TimestampOverflow)
    }
}

pub fn is_same_anchor_opening(input: &[u8]) -> bool {
    SameAnchorOpening::decode_calldata(input).is_ok()
}

fn same_anchor_selector() -> [u8; 4] {
    keccak256(SAME_ANCHOR_SIGNATURE)[..4]
        .try_into()
        .expect("four-byte selector")
}

fn push_word(output: &mut Vec<u8>, value: U256) {
    output.extend_from_slice(&value.to_be_bytes::<32>());
}

fn decode_u64_word(word: &[u8]) -> Result<u64, SameAnchorError> {
    if word.len() != 32 || word[..24].iter().any(|byte| *byte != 0) {
        return Err(SameAnchorError::InvalidCalldata);
    }
    Ok(u64::from_be_bytes(
        word[24..].try_into().expect("validated word length"),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SameAnchorError {
    #[error("unsupported same-anchor opening format {0}")]
    UnsupportedFormat(u8),
    #[error("invalid same-anchor opening calldata")]
    InvalidCalldata,
    #[error("invalid anchor millisecond component {0}")]
    InvalidAnchorMillis(u16),
    #[error("invalid block millisecond component {0}")]
    InvalidBlockMillis(u64),
    #[error("same-anchor timestamp arithmetic overflow")]
    TimestampOverflow,
    #[error("same-anchor timestamp must increase: parent {parent}, got {actual}")]
    TimestampNotIncreasing { parent: u64, actual: u64 },
    #[error("same-anchor timestamp exceeds anchor bound {maximum}: got {actual}")]
    AnchorExpired { maximum: u64, actual: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opening() -> SameAnchorOpening {
        SameAnchorOpening::v1(7, B256::with_last_byte(9), 10, 250, 3)
    }

    #[test]
    fn calldata_round_trip_is_canonical() {
        let encoded = opening().encode_calldata().unwrap();
        assert_eq!(SameAnchorOpening::decode_calldata(&encoded), Ok(opening()));
        assert_eq!(encoded, opening().encode_calldata().unwrap());
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert_eq!(
            SameAnchorOpening::decode_calldata(&trailing),
            Err(SameAnchorError::InvalidCalldata)
        );
    }

    #[test]
    fn rejects_unknown_format_and_stale_timestamp() {
        let mut unknown = opening();
        unknown.format = 2;
        assert_eq!(
            unknown.validate(),
            Err(SameAnchorError::UnsupportedFormat(2))
        );
        assert!(opening().validate_block_timestamp(10_250, 10, 251).is_ok());
        assert_eq!(
            opening().validate_block_timestamp(10_250, 12, 251),
            Err(SameAnchorError::AnchorExpired {
                maximum: 12_250,
                actual: 12_251,
            })
        );
    }
}
