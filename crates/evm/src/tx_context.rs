//! Transaction-hash execution context for authenticated withdrawals.
//!
//! The zone outbox needs the real hash of the currently executing user transaction so it can
//! commit `senderTag = keccak256(sender || txHash)` on-chain. The block executor publishes that
//! hash into a thread-local context before EVM execution, and this precompile exposes it to
//! Solidity at a fixed system address.

use std::{cell::RefCell, thread_local};

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{B256, Bytes, keccak256};
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

fn synthetic_tx_hash(input: &PrecompileInput<'_>) -> B256 {
    let mut bytes = Vec::with_capacity(16 + 20 + 20 + 32 + 32 + 32 + input.data.len());
    bytes.extend_from_slice(b"zone-tx-context");
    bytes.extend_from_slice(input.caller.as_slice());
    bytes.extend_from_slice(input.target_address.as_slice());
    bytes.extend_from_slice(&input.value.to_be_bytes::<32>());
    bytes.extend_from_slice(&input.internals.block_number().to_be_bytes::<32>());
    bytes.extend_from_slice(&input.internals.block_timestamp().to_be_bytes::<32>());
    bytes.extend_from_slice(input.data);
    keccak256(bytes)
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

            let tx_hash = current_tx_hash().unwrap_or_else(|| synthetic_tx_hash(&input));
            let encoded = currentTxHashCall::abi_encode_returns(&tx_hash);
            Ok(PrecompileOutput::new(20, encoded.into(), input.reservoir))
        })
    }
}
