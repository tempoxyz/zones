//! Zone block transaction-order validation.

use alloy_consensus::Transaction;
use alloy_primitives::TxKind;
use alloy_sol_types::SolCall;
use tempo_primitives::TempoTxEnvelope;
use tempo_zone_contracts::ZoneInbox;
use thiserror::Error;
use zone_primitives::constants::ZONE_INBOX_ADDRESS;

/// Invalid required `advanceTempo` transaction sequence.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AdvanceTempoValidationError {
    /// The block contains no transactions.
    #[error("block is missing the required advanceTempo transaction")]
    Missing,
    /// Transaction zero is not a canonical Tempo system transaction.
    #[error("transaction zero must use the canonical Tempo system transaction envelope")]
    InvalidSystemEnvelope,
    /// Transaction zero does not call the Zone inbox.
    #[error("transaction zero must call the ZoneInbox advanceTempo entrypoint")]
    InvalidTarget,
    /// Transaction zero does not select `advanceTempo`.
    #[error("transaction zero must select ZoneInbox.advanceTempo")]
    InvalidSelector,
    /// The `advanceTempo` calldata does not ABI-decode.
    #[error("invalid advanceTempo calldata: {0}")]
    InvalidCalldata(String),
    /// A later transaction attempts another advancement.
    #[error("duplicate advanceTempo transaction at index {index}")]
    Duplicate {
        /// Zero-based transaction index.
        index: usize,
    },
}

fn calls_advance_tempo(tx: &TempoTxEnvelope) -> bool {
    matches!(tx.kind(), TxKind::Call(to) if to == ZONE_INBOX_ADDRESS)
        && tx
            .input()
            .starts_with(&ZoneInbox::advanceTempoCall::SELECTOR)
}

/// Validates that the block starts with exactly one canonical `advanceTempo` system transaction.
///
/// Tempo's reserved system signature uniquely identifies the zero-address system sender, so
/// [`TempoTxEnvelope::is_system_tx`] validates both the envelope signature and recovered sender
/// convention used by block execution.
pub fn validate_advance_tempo_transactions(
    transactions: &[TempoTxEnvelope],
) -> Result<(), AdvanceTempoValidationError> {
    let first = transactions
        .first()
        .ok_or(AdvanceTempoValidationError::Missing)?;
    if !first.is_system_tx() {
        return Err(AdvanceTempoValidationError::InvalidSystemEnvelope);
    }
    if !matches!(first.kind(), TxKind::Call(to) if to == ZONE_INBOX_ADDRESS) {
        return Err(AdvanceTempoValidationError::InvalidTarget);
    }
    if !first
        .input()
        .starts_with(&ZoneInbox::advanceTempoCall::SELECTOR)
    {
        return Err(AdvanceTempoValidationError::InvalidSelector);
    }
    ZoneInbox::advanceTempoCall::abi_decode(first.input())
        .map_err(|err| AdvanceTempoValidationError::InvalidCalldata(err.to_string()))?;

    if let Some((index, _)) = transactions
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, tx)| calls_advance_tempo(tx))
    {
        return Err(AdvanceTempoValidationError::Duplicate { index });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{Address, Bytes, U256};
    use tempo_primitives::transaction::envelope::TEMPO_SYSTEM_TX_SIGNATURE;

    fn advance_tx() -> TempoTxEnvelope {
        let input = ZoneInbox::advanceTempoCall {
            header: Bytes::new(),
            deposits: Vec::new(),
            decryptions: Vec::new(),
            enabledTokens: Vec::new(),
        }
        .abi_encode()
        .into();
        TempoTxEnvelope::Legacy(Signed::new_unhashed(
            TxLegacy {
                chain_id: None,
                nonce: 0,
                gas_price: 0,
                gas_limit: 0,
                to: ZONE_INBOX_ADDRESS.into(),
                value: U256::ZERO,
                input,
            },
            TEMPO_SYSTEM_TX_SIGNATURE,
        ))
    }

    fn regular_tx() -> TempoTxEnvelope {
        TempoTxEnvelope::Legacy(Signed::new_unhashed(
            TxLegacy {
                to: Address::repeat_byte(0x11).into(),
                ..Default::default()
            },
            alloy_primitives::Signature::test_signature(),
        ))
    }

    #[test]
    fn requires_advance_at_transaction_zero() {
        assert_eq!(
            validate_advance_tempo_transactions(&[]),
            Err(AdvanceTempoValidationError::Missing)
        );
        assert_eq!(
            validate_advance_tempo_transactions(&[regular_tx()]),
            Err(AdvanceTempoValidationError::InvalidSystemEnvelope)
        );
        assert!(validate_advance_tempo_transactions(&[advance_tx()]).is_ok());
    }

    #[test]
    fn rejects_duplicate_advance() {
        assert_eq!(
            validate_advance_tempo_transactions(&[advance_tx(), regular_tx(), advance_tx(),]),
            Err(AdvanceTempoValidationError::Duplicate { index: 2 })
        );
    }

    #[test]
    fn rejects_malformed_advance_calldata() {
        let mut tx = advance_tx();
        let TempoTxEnvelope::Legacy(signed) = &mut tx else {
            unreachable!()
        };
        signed.tx_mut().input = ZoneInbox::advanceTempoCall::SELECTOR.to_vec().into();
        assert!(matches!(
            validate_advance_tempo_transactions(&[tx]),
            Err(AdvanceTempoValidationError::InvalidCalldata(_))
        ));
    }
}
