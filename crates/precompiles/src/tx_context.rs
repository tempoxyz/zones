//! Transaction execution context for authenticated withdrawals.
//!
//! The zone outbox needs the real hash, caller, and effective fee payer of the executing user
//! transaction. The Zone EVM publishes them into a thread-local context before EVM execution for
//! the native outbox to read.

use std::{cell::RefCell, thread_local};

use alloy_primitives::{Address, B256};

thread_local! {
    static CURRENT_TRANSACTION: RefCell<Option<TransactionContext>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy)]
struct TransactionContext {
    tx_hash: B256,
    caller: Address,
    fee_payer: Address,
}

/// Guard that clears the current transaction context when dropped.
pub struct TransactionContextGuard;

impl Drop for TransactionContextGuard {
    fn drop(&mut self) {
        CURRENT_TRANSACTION.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Publish the current transaction hash, caller, and effective fee payer for EVM execution.
pub fn set_current_transaction(
    tx_hash: B256,
    caller: Address,
    fee_payer: Address,
) -> TransactionContextGuard {
    CURRENT_TRANSACTION.with(|slot| {
        *slot.borrow_mut() = Some(TransactionContext {
            tx_hash,
            caller,
            fee_payer,
        });
    });
    TransactionContextGuard
}

/// Return the current transaction hash, caller, and effective fee payer, when published by the EVM.
pub(crate) fn current_transaction() -> Option<(B256, Address, Address)> {
    CURRENT_TRANSACTION.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|context| (context.tx_hash, context.caller, context.fee_payer))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_and_clears_current_transaction() {
        let tx_hash = B256::repeat_byte(0x42);
        let caller = Address::repeat_byte(0x12);
        let fee_payer = Address::repeat_byte(0x24);

        let guard = set_current_transaction(tx_hash, caller, fee_payer);
        assert_eq!(current_transaction(), Some((tx_hash, caller, fee_payer)));

        drop(guard);
        assert_eq!(current_transaction(), None, "guard must clear the context");
    }
}
