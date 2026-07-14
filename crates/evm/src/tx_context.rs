//! Transaction-hash execution context for authenticated withdrawals.
//!
//! The zone outbox needs the real hash of the currently executing user transaction so it can
//! commit `senderTag = keccak256(sender || txHash)` on-chain. The block executor publishes that
//! hash into a thread-local context before EVM execution, and this precompile exposes it to
//! Solidity at a fixed system address.

use std::{cell::RefCell, thread_local};

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{B256, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileId, PrecompileOutput};
use tracing::{debug, warn};

alloy_sol_types::sol! {
    function currentTxHash() external returns (bytes32);
    error DelegateCallNotAllowed();
}

thread_local! {
    static CURRENT_TX_HASH: RefCell<Option<B256>> = const { RefCell::new(None) };
}

/// Guard that clears the current tx hash when dropped.
pub(crate) struct TxHashGuard;

impl Drop for TxHashGuard {
    fn drop(&mut self) {
        clear_current_tx_hash();
    }
}

/// Publish the current executing transaction hash for the duration of EVM execution.
pub(crate) fn set_current_tx_hash(tx_hash: B256) -> TxHashGuard {
    CURRENT_TX_HASH.with(|slot| {
        *slot.borrow_mut() = Some(tx_hash);
    });
    TxHashGuard
}

fn clear_current_tx_hash() {
    CURRENT_TX_HASH.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

fn current_tx_hash() -> Option<B256> {
    CURRENT_TX_HASH.with(|slot| *slot.borrow())
}

/// `DynPrecompile` implementation that returns the currently executing zone tx hash.
pub(crate) struct ZoneTxContext;

impl ZoneTxContext {
    pub(crate) fn create() -> DynPrecompile {
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

    fn call_with_tx_hash(tx_hash: Option<B256>) -> PrecompileOutput {
        let _guard = tx_hash.map(set_current_tx_hash);
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
        let output = call_with_tx_hash(Some(tx_hash));

        assert!(!output.is_revert());
        assert_eq!(
            output.bytes,
            currentTxHashCall::abi_encode_returns(&tx_hash)
        );
    }

    #[test]
    fn reverts_when_current_transaction_hash_is_not_set() {
        let output = call_with_tx_hash(None);

        assert!(output.is_revert());
        assert!(output.bytes.is_empty());
    }
}
