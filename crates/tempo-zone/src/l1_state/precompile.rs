//! `DynPrecompile` implementation for the TempoStateReader.
//!
//! The TempoStateReader is a **standalone precompile** (separate from the TempoState contract)
//! that allows zone system contracts to read Tempo L1 contract storage at a specific block height
//! during EVM execution. The caller provides the L1 block number to query, making the precompile
//! fully stateless.
//!
//! This precompile implements two functions:
//!
//! - `readStorageAt(address account, bytes32 slot, uint64 blockNumber) → bytes32`
//! - `readStorageBatchAt(address account, bytes32[] slots, uint64 blockNumber) → bytes32[]`
//!
//! Reads are served synchronously from the [`L1StateProvider`]. The provider first checks the
//! in-memory cache and, on miss, retries the RPC fetch (`eth_getStorageAt` at the given block
//! number) to Tempo L1 indefinitely with exponential backoff. This means a transient L1 RPC
//! outage will stall block production until connectivity is restored, rather than bricking the
//! chain with an unrecoverable hard error.
//!
//! [`PrecompileError`]: revm::precompile::PrecompileError
//!
//! # Gas costs
//!
//! Each call is charged [`BASE_GAS`] plus [`PER_SLOT_GAS`] for every slot read.
//!
//! [`L1StateProvider`]: super::provider::L1StateProvider

use std::sync::Arc;

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Bytes;
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};
use tracing::{debug, error, warn};

use super::L1StorageReader;

alloy_sol_types::sol! {
    /// Read a single storage slot from a Tempo L1 contract at a specific block height.
    function readStorageAt(address account, bytes32 slot, uint64 blockNumber) external view returns (bytes32);

    /// Read multiple storage slots from a Tempo L1 contract at a specific block height.
    function readStorageBatchAt(address account, bytes32[] calldata slots, uint64 blockNumber) external view returns (bytes32[] memory);

    /// Returned when the precompile is invoked via `DELEGATECALL` instead of `CALL`.
    error DelegateCallNotAllowed();
}

/// Fixed gas cost charged on every call.
const BASE_GAS: u64 = 200;

/// Additional gas charged per storage slot read.
const PER_SLOT_GAS: u64 = 200;

/// Factory for the TempoStateReader `DynPrecompile`.
///
/// The precompile is registered at a dedicated predeploy address (separate from the TempoState
/// contract) and handles `readStorageAt` and `readStorageBatchAt` calls by reading Tempo L1
/// contract storage via an [`L1StateProvider`].
///
/// The caller provides the L1 block number to query, making the precompile fully stateless.
/// Zone system contracts (ZoneInbox, ZoneConfig) pass the `tempoBlockNumber` from the
/// TempoState contract after `finalizeTempo` has been called.
///
/// # Restrictions
///
/// - Only direct `CALL`s are accepted; `DELEGATECALL` reverts with [`DelegateCallNotAllowed`].
/// - The precompile is **view-only** — it never writes to EVM state.
/// - On cache miss the provider retries the RPC fetch indefinitely with backoff, stalling
///   block production until L1 connectivity is restored.
pub struct TempoStateReader;

impl TempoStateReader {
    /// Create a [`DynPrecompile`] that dispatches `readStorageAt` and
    /// `readStorageBatchAt` calls to the given [`L1StorageReader`].
    ///
    /// The returned precompile captures `provider` by `Arc` and can be registered in a
    /// [`PrecompilesMap`](alloy_evm::precompiles::PrecompilesMap) at the TempoStateReader
    /// predeploy address.
    pub fn create(provider: Arc<dyn L1StorageReader>) -> DynPrecompile {
        DynPrecompile::new_stateful(
            PrecompileId::Custom("TempoStateReader".into()),
            move |input| {
                if !input.is_direct_call() {
                    warn!(target: "zone::precompile", "TempoStateReader called via DELEGATECALL — rejecting");
                    return Ok(PrecompileOutput::new_reverted(
                        0,
                        DelegateCallNotAllowed {}.abi_encode().into(),
                    ));
                }

                let data = input.data;
                if data.len() < 4 {
                    warn!(target: "zone::precompile", data_len = data.len(), "TempoStateReader called with insufficient data");
                    return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
                }

                let selector: [u8; 4] = data[..4].try_into().expect("len >= 4");

                let result = if selector == readStorageAtCall::SELECTOR {
                    debug!(target: "zone::precompile", "TempoStateReader: readStorageAt");
                    Self::handle_single_slot(provider.as_ref(), data)
                } else if selector == readStorageBatchAtCall::SELECTOR {
                    debug!(target: "zone::precompile", "TempoStateReader: readStorageBatchAt");
                    Self::handle_multi_slot(provider.as_ref(), data)
                } else {
                    warn!(target: "zone::precompile", selector = ?selector, "TempoStateReader: unknown selector");
                    Ok(PrecompileOutput::new_reverted(0, Bytes::new()))
                };

                match &result {
                    Ok(output) if output.bytes.is_empty() && output.gas_used == 0 => {
                        warn!(target: "zone::precompile", "TempoStateReader returned reverted output");
                    }
                    Err(e) => {
                        error!(target: "zone::precompile", %e, "TempoStateReader hard error");
                    }
                    _ => {}
                }

                result
            },
        )
    }

    /// Handle a `readStorageAt(address, bytes32, uint64)` call.
    ///
    /// Decodes the ABI calldata, performs a synchronous lookup via the provider at the specified
    /// L1 block number (cache first, then RPC fallback), and returns the ABI-encoded `bytes32`
    /// value. Returns a hard [`PrecompileError`] if both the cache and RPC fallback fail.
    fn handle_single_slot(provider: &dyn L1StorageReader, data: &[u8]) -> PrecompileResult {
        let call = readStorageAtCall::abi_decode(data)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        let gas = BASE_GAS + PER_SLOT_GAS;

        let value = provider
            .get_storage(call.account, call.slot, call.blockNumber)
            .map_err(|e| {
                zone_precompiles::zone_rpc_error(format!(
                    "L1 storage unavailable for account={} slot={} block={}: {e}",
                    call.account, call.slot, call.blockNumber
                ))
            })?;

        let encoded = readStorageAtCall::abi_encode_returns(&value);
        Ok(PrecompileOutput::new(gas, encoded.into()))
    }

    /// Handle a `readStorageBatchAt(address, bytes32[], uint64)` call.
    ///
    /// Decodes the ABI calldata, performs a synchronous lookup for each slot at the specified
    /// L1 block number (cache first, then RPC fallback), and returns the ABI-encoded `bytes32[]`
    /// result. If **any** slot fails both cache and RPC lookup, the entire call fails with a
    /// hard [`PrecompileError`].
    fn handle_multi_slot(provider: &dyn L1StorageReader, data: &[u8]) -> PrecompileResult {
        let call = readStorageBatchAtCall::abi_decode(data)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        let num_slots = call.slots.len() as u64;
        let gas = BASE_GAS + PER_SLOT_GAS * num_slots;

        let mut results = Vec::with_capacity(call.slots.len());
        for slot in &call.slots {
            let value = provider
                .get_storage(call.account, *slot, call.blockNumber)
                .map_err(|e| {
                    zone_precompiles::zone_rpc_error(format!(
                        "L1 storage unavailable for account={} slot={} block={}: {e}",
                        call.account, slot, call.blockNumber
                    ))
                })?;
            results.push(value);
        }

        let encoded = readStorageBatchAtCall::abi_encode_returns(&results);
        Ok(PrecompileOutput::new(gas, encoded.into()))
    }
}
