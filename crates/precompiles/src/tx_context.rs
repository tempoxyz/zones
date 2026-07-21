//! Transaction execution context shared by the zone EVM and native precompiles.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileId, PrecompileOutput};

alloy_sol_types::sol! {
    function currentTxHash() external returns (bytes32);
    error DelegateCallNotAllowed();
}

#[cfg(feature = "std")]
use std::{cell::RefCell, thread_local};

#[cfg(feature = "std")]
thread_local! {
    static CURRENT_TRANSACTION: RefCell<Option<TransactionContext>> = const { RefCell::new(None) };
}

/// Host-supplied context for the transaction currently executing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(feature = "std")]
struct TransactionContext {
    tx_hash: B256,
    fee_payer: Address,
}

/// Guard that clears the published transaction context when dropped.
#[cfg(feature = "std")]
pub struct TransactionContextGuard;

#[cfg(feature = "std")]
impl Drop for TransactionContextGuard {
    fn drop(&mut self) {
        CURRENT_TRANSACTION.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Publish the authenticated transaction hash and effective fee payer for EVM execution.
#[cfg(feature = "std")]
pub fn set_current_transaction(tx_hash: B256, fee_payer: Address) -> TransactionContextGuard {
    CURRENT_TRANSACTION.with(|slot| {
        *slot.borrow_mut() = Some(TransactionContext { tx_hash, fee_payer });
    });
    TransactionContextGuard
}

/// Return the hash of the transaction currently executing, when supplied by the host.
#[cfg(feature = "std")]
pub fn current_tx_hash() -> Option<B256> {
    CURRENT_TRANSACTION.with(|slot| slot.borrow().as_ref().map(|ctx| ctx.tx_hash))
}

/// Return the effective fee payer of the transaction currently executing.
#[cfg(feature = "std")]
pub fn current_fee_payer() -> Option<Address> {
    CURRENT_TRANSACTION.with(|slot| slot.borrow().as_ref().map(|ctx| ctx.fee_payer))
}

/// Prover execution currently has no thread-local host context.
#[cfg(not(feature = "std"))]
pub fn current_tx_hash() -> Option<B256> {
    None
}

/// Prover execution currently has no thread-local host context.
#[cfg(not(feature = "std"))]
pub fn current_fee_payer() -> Option<Address> {
    None
}

/// Create the transaction-context precompile. Calls without a transaction published by the
/// execution host revert rather than inventing an identifier.
pub fn create_precompile() -> DynPrecompile {
    DynPrecompile::new_stateful(PrecompileId::Custom("ZoneTxContext".into()), |input| {
        if !input.is_direct_call() {
            return Ok(PrecompileOutput::revert(
                0,
                DelegateCallNotAllowed {}.abi_encode().into(),
                input.reservoir,
            ));
        }
        if input.data.len() < 4 || input.data[..4] != currentTxHashCall::SELECTOR {
            return Ok(PrecompileOutput::revert(0, Bytes::new(), input.reservoir));
        }
        let Some(hash) = current_tx_hash() else {
            return Ok(PrecompileOutput::revert(0, Bytes::new(), input.reservoir));
        };
        Ok(PrecompileOutput::new(
            20,
            currentTxHashCall::abi_encode_returns(&hash).into(),
            input.reservoir,
        ))
    })
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use alloy_evm::{
        EvmInternals,
        precompiles::{Precompile, PrecompileInput},
    };
    use alloy_primitives::{Address, U256};
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
        let _guard = context.map(|(hash, fee_payer)| set_current_transaction(hash, fee_payer));
        let mut ctx: TestContext =
            Context::new(CacheDB::new(EmptyDB::new()), TempoHardfork::default());
        let calldata = currentTxHashCall {}.abi_encode();

        create_precompile()
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
        assert_eq!(current_fee_payer(), None, "guard must clear the context");
    }

    #[test]
    fn reverts_when_current_transaction_hash_is_not_set() {
        let output = call_with_context(None);
        assert!(output.is_revert());
        assert!(output.bytes.is_empty());
    }
}
