//! `DynPrecompile` implementation for the TempoState predeploy
//! (`0x1c00000000000000000000000000000000000000`).
//!
//! The TempoState predeploy allows zone contracts to read Tempo L1 contract storage during EVM
//! execution. This module implements the two view functions from the `ITempoState` interface:
//!
//! - `readTempoStorageSlot(address account, bytes32 slot) → bytes32`
//! - `readTempoStorageSlots(address account, bytes32[] slots) → bytes32[]`
//!
//! Reads are served synchronously from the [`L1StateProvider`]. The provider first checks the
//! in-memory cache and, on miss, attempts an RPC fetch to L1 with a configurable timeout. If
//! both the cache and RPC fallback fail, the precompile returns a hard [`PrecompileError`] that
//! halts the entire transaction — this is **not** a catchable revert.
//!
//! [`PrecompileError`]: revm::precompile::PrecompileError
//!
//! # Gas costs
//!
//! Each call is charged [`BASE_GAS`] plus [`PER_SLOT_GAS`] for every slot read.
//!
//! [`L1StateProvider`]: super::provider::L1StateProvider

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Bytes;
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};

use super::provider::L1StateProvider;

alloy_sol_types::sol! {
    /// Read a single storage slot from a Tempo L1 contract.
    ///
    /// Maps to `ITempoState.readTempoStorageSlot`.
    function readTempoStorageSlot(address account, bytes32 slot) external view returns (bytes32);

    /// Read multiple storage slots from a Tempo L1 contract in a single call.
    ///
    /// Maps to `ITempoState.readTempoStorageSlots`.
    function readTempoStorageSlots(address account, bytes32[] calldata slots) external view returns (bytes32[] memory);

    /// Returned when the precompile is invoked via `DELEGATECALL` instead of `CALL`.
    error DelegateCallNotAllowed();

}

/// Fixed gas cost charged on every `readTempoStorageSlot` / `readTempoStorageSlots` call.
const BASE_GAS: u64 = 200;

/// Additional gas charged per storage slot read.
const PER_SLOT_GAS: u64 = 200;

/// Factory for the TempoState `DynPrecompile`.
///
/// The precompile is registered at the TempoState predeploy address
/// (`0x1c00000000000000000000000000000000000000`) and handles `readTempoStorageSlot` and
/// `readTempoStorageSlots` calls by reading from an [`L1StateProvider`].
///
/// # Restrictions
///
/// - Only direct `CALL`s are accepted; `DELEGATECALL` reverts with [`DelegateCallNotAllowed`].
/// - The precompile is **view-only** — it never writes to EVM state.
/// - On cache miss the provider attempts an RPC fetch with timeout; if that also fails the
///   precompile returns a hard [`PrecompileError`] that halts the transaction.
pub struct TempoStatePrecompile;

impl TempoStatePrecompile {
    /// Create a [`DynPrecompile`] that dispatches `readTempoStorageSlot` and
    /// `readTempoStorageSlots` calls to the given [`L1StateProvider`].
    ///
    /// The returned precompile captures `provider` by move and can be registered in a
    /// [`PrecompilesMap`](alloy_evm::precompiles::PrecompilesMap) at the TempoState address.
    pub fn create(provider: L1StateProvider) -> DynPrecompile {
        DynPrecompile::new_stateful(
            PrecompileId::Custom("TempoState".into()),
            move |input| {
                if !input.is_direct_call() {
                    return Ok(PrecompileOutput::new_reverted(
                        0,
                        DelegateCallNotAllowed {}.abi_encode().into(),
                    ));
                }

                let data = input.data;
                if data.len() < 4 {
                    return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
                }

                let selector: [u8; 4] = data[..4].try_into().expect("len >= 4");

                if selector == readTempoStorageSlotCall::SELECTOR {
                    Self::handle_single_slot(&provider, data)
                } else if selector == readTempoStorageSlotsCall::SELECTOR {
                    Self::handle_multi_slot(&provider, data)
                } else {
                    Ok(PrecompileOutput::new_reverted(0, Bytes::new()))
                }
            },
        )
    }

    /// Handle a `readTempoStorageSlot(address, bytes32)` call.
    ///
    /// Decodes the ABI calldata, performs a synchronous lookup via the provider (cache first,
    /// then RPC fallback), and returns the ABI-encoded `bytes32` value. Returns a hard
    /// [`PrecompileError`] if both the cache and RPC fallback fail — this halts the entire
    /// transaction rather than producing a catchable revert.
    fn handle_single_slot(provider: &L1StateProvider, data: &[u8]) -> PrecompileResult {
        let call = readTempoStorageSlotCall::abi_decode(data)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        let gas = BASE_GAS + PER_SLOT_GAS;

        let value = provider.get_storage(call.account, call.slot).map_err(|e| {
            PrecompileError::other(format!(
                "L1 storage unavailable for account={} slot={}: {e}",
                call.account, call.slot
            ))
        })?;

        let encoded = readTempoStorageSlotCall::abi_encode_returns(&value);
        Ok(PrecompileOutput::new(gas, encoded.into()))
    }

    /// Handle a `readTempoStorageSlots(address, bytes32[])` call.
    ///
    /// Decodes the ABI calldata, performs a synchronous lookup for each slot (cache first, then
    /// RPC fallback), and returns the ABI-encoded `bytes32[]` result. If **any** slot fails
    /// both cache and RPC lookup, the entire call fails with a hard [`PrecompileError`].
    fn handle_multi_slot(provider: &L1StateProvider, data: &[u8]) -> PrecompileResult {
        let call = readTempoStorageSlotsCall::abi_decode(data)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        let num_slots = call.slots.len() as u64;
        let gas = BASE_GAS + PER_SLOT_GAS * num_slots;

        let mut results = Vec::with_capacity(call.slots.len());
        for slot in &call.slots {
            let value = provider.get_storage(call.account, *slot).map_err(|e| {
                PrecompileError::other(format!(
                    "L1 storage unavailable for account={} slot={}: {e}",
                    call.account, slot
                ))
            })?;
            results.push(value);
        }

        let encoded = readTempoStorageSlotsCall::abi_encode_returns(&results);
        Ok(PrecompileOutput::new(gas, encoded.into()))
    }
}
