//! Zone block transaction ordering.

use alloy_primitives::TxKind;
use alloy_sol_types::SolCall;
use revm::context::Transaction;
use tempo_revm::{TempoInvalidTransaction, TempoTxEnv};
use tempo_zone_contracts::{ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZoneInbox, ZoneOutbox};

/// A transaction's role in a zone block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZoneBlockTransactionKind {
    AdvanceTempo,
    FinalizeWithdrawalBatch,
    PendingWithdrawalsView,
    User,
}

/// Ordering state derived only from transactions committed to the block.
#[derive(Debug, Default)]
pub(crate) struct ZoneBlockSequence {
    saw_advance_tempo: bool,
    saw_finalize_withdrawal_batch: bool,
    committed_pending_withdrawals_view: bool,
}

impl ZoneBlockSequence {
    /// Check whether `kind` can be the next committed block transaction.
    pub(crate) fn validate_next(
        &self,
        kind: ZoneBlockTransactionKind,
        transaction_index: usize,
        transaction_count_hint: Option<usize>,
    ) -> Result<(), TempoInvalidTransaction> {
        if self.committed_pending_withdrawals_view {
            return Err(invalid(
                "getPendingWithdrawals is a non-committing builder simulation",
            ));
        }

        match kind {
            ZoneBlockTransactionKind::PendingWithdrawalsView => Ok(()),
            ZoneBlockTransactionKind::AdvanceTempo => {
                if self.saw_advance_tempo || transaction_index != 0 {
                    return Err(invalid("advanceTempo must appear exactly once and first"));
                }
                Ok(())
            }
            ZoneBlockTransactionKind::FinalizeWithdrawalBatch => {
                if !self.saw_advance_tempo {
                    return Err(invalid(
                        "finalizeWithdrawalBatch cannot precede advanceTempo",
                    ));
                }
                if self.saw_finalize_withdrawal_batch {
                    return Err(invalid("finalizeWithdrawalBatch may appear at most once"));
                }
                if transaction_count_hint
                    .is_some_and(|count| transaction_index.saturating_add(1) != count)
                {
                    return Err(invalid(
                        "finalizeWithdrawalBatch must be the last transaction",
                    ));
                }
                Ok(())
            }
            ZoneBlockTransactionKind::User => {
                if !self.saw_advance_tempo {
                    return Err(invalid("advanceTempo must be the first transaction"));
                }
                if self.saw_finalize_withdrawal_batch {
                    return Err(invalid("no transaction may follow finalizeWithdrawalBatch"));
                }
                Ok(())
            }
        }
    }

    /// Record a successfully committed transaction.
    pub(crate) fn commit(&mut self, kind: ZoneBlockTransactionKind) {
        match kind {
            ZoneBlockTransactionKind::AdvanceTempo => self.saw_advance_tempo = true,
            ZoneBlockTransactionKind::FinalizeWithdrawalBatch => {
                self.saw_finalize_withdrawal_batch = true;
            }
            ZoneBlockTransactionKind::PendingWithdrawalsView => {
                self.committed_pending_withdrawals_view = true;
            }
            ZoneBlockTransactionKind::User => {}
        }
    }

    /// Enforce the required end-of-block state.
    pub(crate) fn finish(&self) -> Result<(), TempoInvalidTransaction> {
        if self.committed_pending_withdrawals_view {
            return Err(invalid(
                "getPendingWithdrawals must not be committed to a zone block",
            ));
        }
        if !self.saw_advance_tempo {
            return Err(invalid(
                "zone block is missing its required advanceTempo transaction",
            ));
        }
        Ok(())
    }
}

/// Parse the block role of a transaction and reject user impersonation of system operations.
pub(crate) fn classify_transaction(
    tx: &TempoTxEnv,
) -> Result<ZoneBlockTransactionKind, TempoInvalidTransaction> {
    if tx.is_system_tx {
        if tx.tempo_tx_env.is_some() {
            return Err(invalid("zone system transactions must be direct calls"));
        }

        return match (tx.kind(), tx.data.get(..4)) {
            (TxKind::Call(ZONE_INBOX_ADDRESS), Some(selector))
                if selector == ZoneInbox::advanceTempoCall::SELECTOR =>
            {
                Ok(ZoneBlockTransactionKind::AdvanceTempo)
            }
            (TxKind::Call(ZONE_OUTBOX_ADDRESS), Some(selector))
                if selector == ZoneOutbox::finalizeWithdrawalBatchCall::SELECTOR =>
            {
                Ok(ZoneBlockTransactionKind::FinalizeWithdrawalBatch)
            }
            (TxKind::Call(ZONE_OUTBOX_ADDRESS), Some(selector))
                if selector == ZoneOutbox::getPendingWithdrawalsCall::SELECTOR =>
            {
                Ok(ZoneBlockTransactionKind::PendingWithdrawalsView)
            }
            _ => Err(invalid("unrecognized zone system transaction")),
        };
    }

    let impersonates_system_operation = if let Some(aa) = tx.tempo_tx_env.as_ref() {
        aa.aa_calls
            .iter()
            .any(|call| is_state_changing_system_operation(call.to, &call.input))
    } else {
        is_state_changing_system_operation(tx.kind(), &tx.data)
    };

    if impersonates_system_operation {
        return Err(invalid(
            "advanceTempo and finalizeWithdrawalBatch require a system transaction",
        ));
    }

    Ok(ZoneBlockTransactionKind::User)
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

const fn invalid(message: &'static str) -> TempoInvalidTransaction {
    TempoInvalidTransaction::CallsValidation(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, U256};
    use tempo_primitives::transaction::Call;
    use tempo_revm::TempoBatchCallEnv;

    fn call_tx(target: alloy_primitives::Address, input: Bytes, is_system_tx: bool) -> TempoTxEnv {
        TempoTxEnv {
            inner: revm::context::TxEnv {
                kind: TxKind::Call(target),
                data: input,
                ..Default::default()
            },
            is_system_tx,
            ..Default::default()
        }
    }

    #[test]
    fn enforces_required_system_transaction_order() {
        let sequence = ZoneBlockSequence::default();
        assert!(
            sequence
                .validate_next(ZoneBlockTransactionKind::User, 0, Some(2))
                .is_err()
        );
        assert!(
            sequence
                .validate_next(
                    ZoneBlockTransactionKind::FinalizeWithdrawalBatch,
                    0,
                    Some(2)
                )
                .is_err()
        );

        let mut sequence = ZoneBlockSequence::default();
        sequence
            .validate_next(ZoneBlockTransactionKind::AdvanceTempo, 0, Some(3))
            .unwrap();
        sequence.commit(ZoneBlockTransactionKind::AdvanceTempo);
        sequence
            .validate_next(ZoneBlockTransactionKind::User, 1, Some(3))
            .unwrap();
        sequence.commit(ZoneBlockTransactionKind::User);
        sequence
            .validate_next(
                ZoneBlockTransactionKind::FinalizeWithdrawalBatch,
                2,
                Some(3),
            )
            .unwrap();
        sequence.commit(ZoneBlockTransactionKind::FinalizeWithdrawalBatch);
        assert!(
            sequence
                .validate_next(ZoneBlockTransactionKind::User, 3, None)
                .is_err()
        );
        sequence.finish().unwrap();
    }

    #[test]
    fn rejects_non_last_finalize_when_block_length_is_known() {
        let mut sequence = ZoneBlockSequence::default();
        sequence.commit(ZoneBlockTransactionKind::AdvanceTempo);
        assert!(
            sequence
                .validate_next(
                    ZoneBlockTransactionKind::FinalizeWithdrawalBatch,
                    1,
                    Some(3)
                )
                .is_err()
        );
    }

    #[test]
    fn permits_view_simulation_but_rejects_a_committed_view() {
        let mut sequence = ZoneBlockSequence::default();
        sequence.commit(ZoneBlockTransactionKind::AdvanceTempo);
        sequence
            .validate_next(ZoneBlockTransactionKind::PendingWithdrawalsView, 1, None)
            .unwrap();
        sequence.finish().unwrap();

        sequence.commit(ZoneBlockTransactionKind::PendingWithdrawalsView);
        assert!(sequence.finish().is_err());
        assert!(
            sequence
                .validate_next(ZoneBlockTransactionKind::User, 2, None)
                .is_err()
        );
    }

    #[test]
    fn classifies_system_calls_and_rejects_user_impersonation() {
        let advance_input = Bytes::copy_from_slice(&ZoneInbox::advanceTempoCall::SELECTOR);
        let system = call_tx(ZONE_INBOX_ADDRESS, advance_input.clone(), true);
        assert_eq!(
            classify_transaction(&system).unwrap(),
            ZoneBlockTransactionKind::AdvanceTempo
        );

        let user = call_tx(ZONE_INBOX_ADDRESS, advance_input.clone(), false);
        assert!(classify_transaction(&user).is_err());

        let aa_user = TempoTxEnv {
            tempo_tx_env: Some(Box::new(TempoBatchCallEnv {
                aa_calls: vec![Call {
                    to: TxKind::Call(ZONE_INBOX_ADDRESS),
                    value: U256::ZERO,
                    input: advance_input,
                }],
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(classify_transaction(&aa_user).is_err());
    }
}
