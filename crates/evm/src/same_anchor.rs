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
pub const T14_FAST_PROTOCOL_NATIVE_PIN: B256 =
    alloy_primitives::b256!("8e57521218d8ec9db175a95bca2c3a2061d54f8b60cd443c2c34735741534f8f");
const SAME_ANCHOR_SIGNATURE: &[u8] = b"sameAnchor(uint8,uint64,bytes32,uint64,uint64,uint16)";
const SAME_ANCHOR_CALLDATA_LEN: usize = 4 + 32 * 6;

/// Capability reconstructed from typed Portal storage at the exact finalized anchor.
///
/// This is deliberately not encoded in [`SameAnchorOpening`]: block input cannot authenticate its
/// own authority. A payload validator or prover may construct this value only from its anchored L1
/// storage/witness implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchoredProtocolEpoch {
    pub tempo_block_number: u64,
    pub current_epoch: u64,
    pub activated_at_tempo_block: u64,
    pub closed: bool,
    pub retired: bool,
    pub proof_mode: u8,
    pub expected_verifier_code_hash: B256,
    pub expected_verifier_config_hash: B256,
    pub native_pin: B256,
    pub registry_schema_valid: bool,
    pub t14_capability_compatible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameAnchorMode {
    Open,
    ClosedDrain,
}

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

    /// Bind the opening epoch to the current unretired T14 capability at this exact anchor.
    ///
    /// `None` is not equivalent to epoch zero: it means the caller cannot authenticate the Portal
    /// read and must reject the block.
    pub fn validate_protocol_epoch(
        &self,
        capability: Option<&AnchoredProtocolEpoch>,
    ) -> Result<SameAnchorMode, SameAnchorError> {
        self.validate()?;
        let capability = capability.ok_or(SameAnchorError::ProtocolEpochUnproven)?;
        if self.protocol_epoch == 0
            || capability.tempo_block_number != self.tempo_block_number
            || capability.current_epoch != self.protocol_epoch
            || capability.activated_at_tempo_block == 0
            || capability.activated_at_tempo_block > self.tempo_block_number
            || capability.retired
            || !matches!(capability.proof_mode, 1 | 2)
            || capability.expected_verifier_code_hash.is_zero()
            || capability.expected_verifier_config_hash.is_zero()
            || capability.native_pin != T14_FAST_PROTOCOL_NATIVE_PIN
            || !capability.registry_schema_valid
            || !capability.t14_capability_compatible
        {
            return Err(SameAnchorError::ProtocolEpochMismatch {
                opening: self.protocol_epoch,
                anchored: capability.current_epoch,
            });
        }
        Ok(if capability.closed {
            SameAnchorMode::ClosedDrain
        } else {
            SameAnchorMode::Open
        })
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
            // Same-anchor is a state-neutral T14 system commitment; validation binds these bytes
            // to the already imported finalized Tempo anchor before ordinary calls execute.
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
    #[error("same-anchor protocol epoch is not proven by authenticated anchor storage")]
    ProtocolEpochUnproven,
    #[error("same-anchor protocol epoch mismatch: opening {opening}, anchored {anchored}")]
    ProtocolEpochMismatch { opening: u64, anchored: u64 },
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

    #[test]
    fn protocol_epoch_requires_exact_unretired_anchored_capability() {
        let opening = opening();
        let valid = AnchoredProtocolEpoch {
            tempo_block_number: 7,
            current_epoch: 3,
            activated_at_tempo_block: 7,
            closed: false,
            retired: false,
            proof_mode: 1,
            expected_verifier_code_hash: B256::repeat_byte(1),
            expected_verifier_config_hash: B256::repeat_byte(2),
            native_pin: T14_FAST_PROTOCOL_NATIVE_PIN,
            registry_schema_valid: true,
            t14_capability_compatible: true,
        };
        assert_eq!(
            opening.validate_protocol_epoch(None),
            Err(SameAnchorError::ProtocolEpochUnproven)
        );
        assert_eq!(
            opening.validate_protocol_epoch(Some(&valid)),
            Ok(SameAnchorMode::Open)
        );
        assert_eq!(
            opening.validate_protocol_epoch(Some(&AnchoredProtocolEpoch {
                closed: true,
                ..valid
            })),
            Ok(SameAnchorMode::ClosedDrain)
        );

        for invalid in [
            AnchoredProtocolEpoch {
                current_epoch: 4,
                ..valid
            },
            AnchoredProtocolEpoch {
                tempo_block_number: 8,
                ..valid
            },
            AnchoredProtocolEpoch {
                activated_at_tempo_block: 8,
                ..valid
            },
            AnchoredProtocolEpoch {
                retired: true,
                ..valid
            },
            AnchoredProtocolEpoch {
                proof_mode: 0,
                ..valid
            },
            AnchoredProtocolEpoch {
                native_pin: B256::ZERO,
                ..valid
            },
            AnchoredProtocolEpoch {
                registry_schema_valid: false,
                ..valid
            },
            AnchoredProtocolEpoch {
                t14_capability_compatible: false,
                ..valid
            },
        ] {
            assert!(matches!(
                opening.validate_protocol_epoch(Some(&invalid)),
                Err(SameAnchorError::ProtocolEpochMismatch { .. })
            ));
        }
    }
}
