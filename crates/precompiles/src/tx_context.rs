//! Transaction execution context for authenticated withdrawals.
//!
//! The zone outbox needs the execution kind and effective fee payer of the current call. Canonical
//! transactions additionally carry their real signed hash; simulations deliberately do not. The
//! native outbox and transaction-context precompile read the same thread-local context.

use std::{cell::RefCell, thread_local};

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileId, PrecompileOutput};
use tracing::{debug, warn};

use crate::outbox::WithdrawalExecutionContext;

alloy_sol_types::sol! {
    function currentTxHash() external returns (bytes32);
    error DelegateCallNotAllowed();
}

thread_local! {
    static CURRENT_EXECUTION: RefCell<Option<WithdrawalExecutionContext>> = const { RefCell::new(None) };
}

/// Guard that clears the current transaction context when dropped.
pub struct TransactionContextGuard;

impl Drop for TransactionContextGuard {
    fn drop(&mut self) {
        CURRENT_EXECUTION.with(|slot| *slot.borrow_mut() = None);
    }
}

fn set_current_execution(context: WithdrawalExecutionContext) -> TransactionContextGuard {
    CURRENT_EXECUTION.with(|slot| *slot.borrow_mut() = Some(context));
    TransactionContextGuard
}

fn set_current_execution_if_unset(
    context: WithdrawalExecutionContext,
) -> Option<TransactionContextGuard> {
    CURRENT_EXECUTION.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return None;
        }

        *slot = Some(context);
        Some(TransactionContextGuard)
    })
}

/// Publish the current transaction hash and effective fee payer for EVM execution.
pub fn set_current_transaction(tx_hash: B256, fee_payer: Address) -> TransactionContextGuard {
    set_current_execution(WithdrawalExecutionContext::Transaction { tx_hash, fee_payer })
}

/// Publish a transaction context when the executor has not already installed one.
///
/// Direct EVM execution uses this for the explicit transaction or simulation context carried by
/// the transaction environment, without overriding canonical block-execution context.
pub fn set_current_transaction_if_unset(
    tx_hash: B256,
    fee_payer: Address,
) -> Option<TransactionContextGuard> {
    set_current_execution_if_unset(WithdrawalExecutionContext::Transaction { tx_hash, fee_payer })
}

/// Publish an explicit simulation context unless the block executor already installed a canonical
/// transaction context.
pub fn set_simulation_if_unset(fee_payer: Address) -> Option<TransactionContextGuard> {
    set_current_execution_if_unset(WithdrawalExecutionContext::Simulation { fee_payer })
}

/// Return the current semantic execution context, when published by the executor or EVM.
pub(crate) fn current_execution() -> Option<WithdrawalExecutionContext> {
    CURRENT_EXECUTION.with(|slot| *slot.borrow())
}

fn current_tx_hash() -> Option<B256> {
    match current_execution()? {
        WithdrawalExecutionContext::Transaction { tx_hash, .. } => Some(tx_hash),
        WithdrawalExecutionContext::Simulation { .. } => None,
    }
}

/// `DynPrecompile` implementation that returns the currently executing zone transaction hash.
pub struct ZoneTxContext;

impl ZoneTxContext {
    /// Creates the native transaction-context precompile.
    pub fn create() -> DynPrecompile {
        DynPrecompile::new_stateful(PrecompileId::Custom("ZoneTxContext".into()), move |input| {
            if !input.is_direct_call() {
                warn!(
                    target: "zone::precompile",
                    "ZoneTxContext called via DELEGATECALL — rejecting"
                );
                return Ok(PrecompileOutput::revert(
                    0,
                    DelegateCallNotAllowed {}.abi_encode().into(),
                    input.reservoir,
                ));
            }

            let data = input.data;
            if data.len() < 4 {
                warn!(
                    target: "zone::precompile",
                    data_len = data.len(),
                    "ZoneTxContext called with insufficient data"
                );
                return Ok(PrecompileOutput::revert(0, Bytes::new(), input.reservoir));
            }

            let selector: [u8; 4] = data[..4].try_into().expect("len >= 4");
            if selector != currentTxHashCall::SELECTOR {
                warn!(
                    target: "zone::precompile",
                    ?selector,
                    "ZoneTxContext: unknown selector"
                );
                return Ok(PrecompileOutput::revert(0, Bytes::new(), input.reservoir));
            }

            debug!(target: "zone::precompile", "ZoneTxContext: currentTxHash");

            let Some(tx_hash) = current_tx_hash() else {
                warn!(
                    target: "zone::precompile",
                    "ZoneTxContext: current transaction hash is not set"
                );
                return Ok(PrecompileOutput::revert(0, Bytes::new(), input.reservoir));
            };
            let encoded = currentTxHashCall::abi_encode_returns(&tx_hash);
            Ok(PrecompileOutput::new(20, encoded.into(), input.reservoir))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_evm::{
        EvmInternals,
        precompiles::{Precompile, PrecompileInput},
    };
    use alloy_primitives::U256;
    use revm::{
        Context,
        database::{CacheDB, EmptyDB},
    };
    use tempo_chainspec::hardfork::TempoHardfork;

    type TestContext = Context<
        revm::context::BlockEnv,
        revm::context::TxEnv,
        revm::context::CfgEnv<TempoHardfork>,
        CacheDB<EmptyDB>,
    >;

    fn call_with_context(context: Option<WithdrawalExecutionContext>) -> PrecompileOutput {
        let _guard = context.map(set_current_execution);
        let mut ctx: TestContext =
            Context::new(CacheDB::new(EmptyDB::new()), TempoHardfork::default());
        let calldata = currentTxHashCall {}.abi_encode();

        ZoneTxContext::create()
            .call(PrecompileInput {
                data: &calldata,
                gas: u64::MAX,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                target_address: Address::ZERO,
                is_static: true,
                bytecode_address: Address::ZERO,
                internals: EvmInternals::from_context(&mut ctx),
            })
            .expect("precompile call should not fail")
    }

    #[test]
    fn returns_current_transaction_hash() {
        let tx_hash = B256::repeat_byte(0x42);
        let fee_payer = Address::repeat_byte(0x24);
        let output = call_with_context(Some(WithdrawalExecutionContext::Transaction {
            tx_hash,
            fee_payer,
        }));

        assert!(!output.is_revert());
        assert_eq!(
            output.bytes,
            currentTxHashCall::abi_encode_returns(&tx_hash)
        );
        assert_eq!(current_execution(), None, "guard must clear the context");
    }

    #[test]
    fn reverts_when_current_transaction_hash_is_not_set() {
        let output = call_with_context(None);

        assert!(output.is_revert());
        assert!(output.bytes.is_empty());
    }

    #[test]
    fn simulation_has_no_transaction_hash() {
        let output = call_with_context(Some(WithdrawalExecutionContext::Simulation {
            fee_payer: Address::repeat_byte(0x24),
        }));

        assert!(output.is_revert());
        assert!(output.bytes.is_empty());
    }

    #[test]
    fn fallback_context_installs_and_clears_when_unset() {
        let tx_hash = B256::repeat_byte(0x42);
        let fee_payer = Address::repeat_byte(0x24);

        let guard = set_current_transaction_if_unset(tx_hash, fee_payer)
            .expect("fallback context should be installed");
        assert_eq!(
            current_execution(),
            Some(WithdrawalExecutionContext::Transaction { tx_hash, fee_payer })
        );

        drop(guard);
        assert_eq!(current_execution(), None);
    }

    #[test]
    fn simulation_context_installs_and_clears_when_unset() {
        let fee_payer = Address::repeat_byte(0x24);

        let guard =
            set_simulation_if_unset(fee_payer).expect("simulation context should be installed");
        assert_eq!(
            current_execution(),
            Some(WithdrawalExecutionContext::Simulation { fee_payer })
        );

        drop(guard);
        assert_eq!(current_execution(), None);
    }

    #[test]
    fn fallback_context_does_not_override_executor_context() {
        let real_hash = B256::repeat_byte(0x42);
        let real_fee_payer = Address::repeat_byte(0x24);
        let _real_guard = set_current_transaction(real_hash, real_fee_payer);

        let fallback_guard =
            set_current_transaction_if_unset(B256::repeat_byte(0x11), Address::repeat_byte(0x22));

        assert!(fallback_guard.is_none());
        assert_eq!(
            current_execution(),
            Some(WithdrawalExecutionContext::Transaction {
                tx_hash: real_hash,
                fee_payer: real_fee_payer,
            })
        );
    }
}
