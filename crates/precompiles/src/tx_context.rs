//! Transaction execution context for authenticated withdrawals.
//!
//! The zone outbox needs the real hash and effective fee payer of the currently executing user
//! transaction. The Zone EVM publishes both into a thread-local context before EVM execution. The
//! native outbox and transaction-context precompile read the same context.

use std::{cell::RefCell, thread_local};

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileId, PrecompileOutput};
use tracing::{debug, warn};

alloy_sol_types::sol! {
    function currentTxHash() external returns (bytes32);
    error DelegateCallNotAllowed();
}

thread_local! {
    static CURRENT_TRANSACTION: RefCell<Option<TransactionContext>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy)]
struct TransactionContext {
    tx_hash: B256,
    fee_payer: Address,
}

/// Guard that clears the current transaction context when dropped.
pub struct TransactionContextGuard;

impl Drop for TransactionContextGuard {
    fn drop(&mut self) {
        CURRENT_TRANSACTION.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Publish the current transaction hash and effective fee payer for EVM execution.
pub fn set_current_transaction(tx_hash: B256, fee_payer: Address) -> TransactionContextGuard {
    CURRENT_TRANSACTION.with(|slot| {
        *slot.borrow_mut() = Some(TransactionContext { tx_hash, fee_payer });
    });
    TransactionContextGuard
}

/// Return the current transaction hash and effective fee payer, when published by the EVM.
pub(crate) fn current_transaction() -> Option<(B256, Address)> {
    CURRENT_TRANSACTION.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|context| (context.tx_hash, context.fee_payer))
    })
}

fn current_tx_hash() -> Option<B256> {
    current_transaction().map(|(tx_hash, _)| tx_hash)
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

    fn call_with_context(context: Option<(B256, Address)>) -> PrecompileOutput {
        let _guard =
            context.map(|(tx_hash, fee_payer)| set_current_transaction(tx_hash, fee_payer));
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
        let output = call_with_context(Some((tx_hash, fee_payer)));

        assert!(!output.is_revert());
        assert_eq!(
            output.bytes,
            currentTxHashCall::abi_encode_returns(&tx_hash)
        );
        assert_eq!(current_transaction(), None, "guard must clear the context");
    }

    #[test]
    fn reverts_when_current_transaction_hash_is_not_set() {
        let output = call_with_context(None);

        assert!(output.is_revert());
        assert!(output.bytes.is_empty());
    }
}
