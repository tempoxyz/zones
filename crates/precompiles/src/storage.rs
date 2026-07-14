//! Zone precompile storage provider backed by finalized Tempo L1 state.
//!
//! Ordinary operations use the zone's local EVM state. Selected policy reads are overlaid from
//! the Tempo L1 block recorded in `TempoState`.
//!
//! # Read behavior
//!
//! - TIP-403 registry slots return the corresponding L1 value.
//! - TIP-20 transfer-policy slots replace only the L1-owned policy-ID field, preserving the
//!   remaining zone-local fields in the packed slot.
//! - All other slots return their zone-local value unchanged.
//!
//! Each mirrored read performs the local SLOAD first to preserve EVM warming, gas charging, and
//! storage-action accounting. Every L1 read during a precompile call uses the same block anchor.
//!
//! # Write behavior
//!
//! Persistent writes, increments, and decrements targeting mirrored state are rejected before
//! reaching the local EVM provider. Writes to all other slots delegate unchanged.

use alloc::format;

pub(crate) use tempo_precompiles::storage::*;

use crate::tempo_state::slots as tempo_state_slots;
use alloy_primitives::{Address, B256, LogData, U256};
use revm::{
    context::journaled_state::JournalCheckpoint,
    precompile::PrecompileError,
    state::{AccountInfo, Bytecode},
};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_contracts::precompiles::{TIP403_REGISTRY_ADDRESS, TIP403RegistryError};
use tempo_precompiles::{
    error::{Result, TempoPrecompileError},
    storage::evm::EvmPrecompileStorageProvider,
    tip20::{TIP20Error, tip20_slots},
};
use tempo_primitives::{TempoAddressExt, TempoBlockEnv};
use zone_primitives::constants::TEMPO_STATE_ADDRESS;

/// L1 storage access needed by zone precompile storage overlays and `TempoState` reads.
pub trait L1StorageReader: Clone + Send + Sync + 'static {
    /// Zone portal account whose configuration is mirrored from Tempo L1.
    fn portal_address(&self) -> Address;

    /// Read `account[slot]` at `block_number` on Tempo L1.
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
    ) -> core::result::Result<B256, PrecompileError>;

    /// Resolve the Tempo hardfork active at `block_number` on L1.
    fn hardfork_at(
        &self,
        block_number: u64,
    ) -> core::result::Result<TempoHardfork, PrecompileError>;
}

/// Precompile storage that overlays finalized Tempo L1 policy state onto zone-local EVM state.
///
/// TIP-403 reads use L1 values, while TIP-20 policy reads replace only the policy-ID field.
/// Ordinary operations remain local, and persistent writes to mirrored state are rejected.
pub struct ZonePrecompileStorageProvider<'a, P> {
    inner: EvmPrecompileStorageProvider<'a>,
    l1_block_number: u64,
    l1_spec: TempoHardfork,
    l1: P,
}

impl<'a, P: L1StorageReader> ZonePrecompileStorageProvider<'a, P> {
    /// Wrap `inner` with an L1 reader bound to `l1_block_number` for this precompile call.
    ///
    /// The L1 hardfork is resolved from the same block here so callers cannot accidentally pair
    /// storage from one anchor with execution rules from another.
    pub fn new(
        inner: EvmPrecompileStorageProvider<'a>,
        l1: P,
        l1_block_number: u64,
    ) -> Result<Self> {
        let l1_spec = l1
            .hardfork_at(l1_block_number)
            .map_err(fatal_reader_error)?;
        Ok(Self {
            inner,
            l1,
            l1_block_number,
            l1_spec,
        })
    }
}

/// Read the finalized Tempo/L1 block number once before constructing the zone provider.
pub fn read_l1_anchor(inner: &mut EvmPrecompileStorageProvider<'_>) -> Result<u64> {
    let value = inner.sload(TEMPO_STATE_ADDRESS, tempo_state_slots::TEMPO_BLOCK_NUMBER)?;
    value.try_into().map_err(|_| {
        TempoPrecompileError::Fatal(format!(
            "invalid Tempo L1 block anchor (does not fit in u64): {value}"
        ))
    })
}

impl<P: L1StorageReader> ZonePrecompileStorageProvider<'_, P> {
    fn read_l1_slot(&self, address: Address, key: U256) -> Result<U256> {
        let block_number = self.l1_block_number;
        self.l1
            .read_l1_storage(address, key.into(), block_number)
            .map(|value| value.into())
            .map_err(|err| trace_err(err, address, key, block_number))
    }
}

impl<P: L1StorageReader> PrecompileStorageProvider for ZonePrecompileStorageProvider<'_, P> {
    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    fn block_env(&self) -> &TempoBlockEnv {
        self.inner.block_env()
    }

    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<()> {
        self.inner.set_code(address, code)
    }

    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<()> {
        self.inner.with_account_info(address, f)
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256> {
        // Run the local SLOAD first to preserve EVM warm/cold state, gas charging, and storage-action
        // recording; mirrored L1 state overrides only the value observed by TIP-20/TIP-403 logic.
        let local = self.inner.sload(address, key)?;
        if address == TIP403_REGISTRY_ADDRESS || address == self.l1.portal_address() {
            return self.read_l1_slot(address, key);
        }
        if is_tip20_policy_id_slot(address, key) {
            let l1 = self.read_l1_slot(address, key)?;
            return Ok(merge_transfer_policy_id(local, l1));
        }
        Ok(local)
    }

    fn tload(&mut self, address: Address, key: U256) -> Result<U256> {
        self.inner.tload(address, key)
    }

    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        if address == TIP403_REGISTRY_ADDRESS
            || address == self.l1.portal_address()
            || is_tip20_policy_id_slot(address, key)
                && value != merge_transfer_policy_id(value, self.read_l1_slot(address, key)?)
        {
            return Err(l1_write_err(address, key));
        }
        self.inner.sstore(address, key, value)
    }

    fn sinc(&mut self, address: Address, key: U256, delta: U256) -> Result<()> {
        if is_l1_slot(address, key) {
            return Err(l1_write_err(address, key));
        }
        self.inner.sinc(address, key, delta)
    }

    fn sdec(&mut self, address: Address, key: U256, delta: U256) -> Result<()> {
        if is_l1_slot(address, key) {
            return Err(l1_write_err(address, key));
        }
        self.inner.sdec(address, key, delta)
    }

    fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        self.inner.tstore(address, key, value)
    }

    fn emit_event(&mut self, address: Address, event: LogData) -> Result<()> {
        self.inner.emit_event(address, event)
    }

    fn deduct_gas(&mut self, gas: u64) -> Result<()> {
        self.inner.deduct_gas(gas)
    }

    fn refund_gas(&mut self, gas: i64) {
        self.inner.refund_gas(gas)
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_used(&self) -> u64 {
        self.inner.gas_used()
    }

    fn state_gas_used(&self) -> u64 {
        self.inner.state_gas_used()
    }

    fn gas_refunded(&self) -> i64 {
        self.inner.gas_refunded()
    }

    fn reservoir(&self) -> u64 {
        self.inner.reservoir()
    }

    fn spec(&self) -> TempoHardfork {
        self.l1_spec
    }

    fn storage_actions(&self) -> StorageActions {
        self.inner.storage_actions()
    }

    fn amsterdam_eip8037_enabled(&self) -> bool {
        self.inner.amsterdam_eip8037_enabled()
    }

    fn is_static(&self) -> bool {
        self.inner.is_static()
    }

    fn checkpoint(&mut self) -> JournalCheckpoint {
        self.inner.checkpoint()
    }

    fn checkpoint_commit(&mut self, checkpoint: JournalCheckpoint) {
        self.inner.checkpoint_commit(checkpoint)
    }

    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        self.inner.checkpoint_revert(checkpoint)
    }

    fn set_tip1060_storage_credits(&mut self, enabled: bool) {
        self.inner.set_tip1060_storage_credits(enabled)
    }

    fn set_tip1060_storage_credit_minting(&mut self, enabled: bool) {
        self.inner.set_tip1060_storage_credit_minting(enabled)
    }
}

// TODO(rusowsky): Remove TIP20 policy-slot detection, write protection, merge logic,
// and related tests once Tempo L1 migrates transfer policy IDs into TIP403.
fn is_l1_slot(address: Address, key: U256) -> bool {
    address == TIP403_REGISTRY_ADDRESS || is_tip20_policy_id_slot(address, key)
}

fn is_tip20_policy_id_slot(address: Address, key: U256) -> bool {
    address.is_tip20() && key == tip20_slots::TRANSFER_POLICY_ID
}

fn l1_write_err(address: Address, key: U256) -> TempoPrecompileError {
    if is_tip20_policy_id_slot(address, key) {
        TIP20Error::invalid_transfer_policy_id().into()
    } else {
        TIP403RegistryError::unauthorized().into()
    }
}

pub(super) fn trace_err(
    err: PrecompileError,
    address: Address,
    key: U256,
    block_number: u64,
) -> TempoPrecompileError {
    TempoPrecompileError::Fatal(format!(
        "{}; Tempo L1 storage read failed address={address} key={key} block={block_number}",
        precompile_error_message(err)
    ))
}

fn fatal_reader_error(err: PrecompileError) -> TempoPrecompileError {
    TempoPrecompileError::Fatal(precompile_error_message(err))
}

fn precompile_error_message(err: PrecompileError) -> alloc::string::String {
    match err {
        PrecompileError::Fatal(msg) => msg,
        other => format!("{other:?}"),
    }
}

// TODO(rusowsky): Remove once Tempo L1 stores transfer policy IDs in the TIP403 precompile.
fn merge_transfer_policy_id(local_slot: U256, l1_slot: U256) -> U256 {
    let offset_bits = tip20_slots::TRANSFER_POLICY_ID_OFFSET * 8;
    let field_bits = core::mem::size_of::<u64>() * 8;
    let field_mask = ((U256::ONE << field_bits) - U256::ONE) << offset_bits;
    (local_slot & !field_mask) | (l1_slot & field_mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockL1Reader, TestContext, test_context, test_storage_provider};
    use tempo_precompiles::{
        PATH_USD_ADDRESS,
        storage::{StorageAction, actions::StorageActions},
    };

    fn with_zone_provider<T>(
        ctx: &mut TestContext,
        l1: MockL1Reader,
        actions: StorageActions,
        f: impl FnOnce(&mut ZonePrecompileStorageProvider<'_, MockL1Reader>) -> T,
    ) -> T {
        let mut inner = test_storage_provider(ctx, u64::MAX, false).with_actions(actions);
        inner
            .sstore(
                TEMPO_STATE_ADDRESS,
                tempo_state_slots::TEMPO_BLOCK_NUMBER,
                U256::from(123u64),
            )
            .expect("anchor write succeeds");
        let l1_block_number = read_l1_anchor(&mut inner).expect("anchor read succeeds");
        let mut provider = ZonePrecompileStorageProvider::new(inner, l1, l1_block_number)
            .expect("hardfork resolution succeeds");
        f(&mut provider)
    }

    #[test]
    fn provider_resolves_hardfork_at_storage_anchor_and_fails_closed() {
        let mut ctx = test_context();
        let l1 = MockL1Reader::default();
        let provider = ZonePrecompileStorageProvider::new(
            test_storage_provider(&mut ctx, u64::MAX, false),
            l1.clone(),
            77,
        )
        .expect("anchored hardfork resolves");
        assert_eq!(provider.spec(), TempoHardfork::T8);
        assert_eq!(l1.hardfork_requests(), vec![77]);
        drop(provider);

        let result = ZonePrecompileStorageProvider::new(
            test_storage_provider(&mut ctx, u64::MAX, false),
            MockL1Reader::failing_hardfork(),
            77,
        );
        let err = match result {
            Err(err) => err,
            Ok(_) => panic!("missing anchored hardfork must fail closed"),
        };
        assert!(matches!(
            err,
            TempoPrecompileError::Fatal(message) if message.contains("hardfork unavailable")
        ));
    }

    #[test]
    fn read_l1_anchor_rejects_values_larger_than_u64() {
        let mut ctx = test_context();
        let mut inner = test_storage_provider(&mut ctx, u64::MAX, false);
        let oversized = U256::from(u64::MAX) + U256::ONE;
        inner
            .sstore(
                TEMPO_STATE_ADDRESS,
                tempo_state_slots::TEMPO_BLOCK_NUMBER,
                oversized,
            )
            .expect("anchor write succeeds");

        let err = read_l1_anchor(&mut inner).expect_err("oversized anchor must be rejected");
        assert!(
            matches!(err, TempoPrecompileError::Fatal(ref msg) if msg.contains("does not fit in u64") && msg.contains(&oversized.to_string()))
        );
    }

    #[test]
    fn l1_read_failure_includes_storage_context() {
        let mut ctx = test_context();
        with_zone_provider(
            &mut ctx,
            MockL1Reader::failing_storage(),
            StorageActions::disabled(),
            |provider| {
                let err = provider
                    .sload(TIP403_REGISTRY_ADDRESS, U256::ONE)
                    .expect_err("L1 failures must fail closed");
                assert!(matches!(
                    err,
                    TempoPrecompileError::Fatal(msg)
                        if crate::is_zone_rpc_error(&msg)
                            && msg.contains(&TIP403_REGISTRY_ADDRESS.to_string())
                            && msg.contains("block=123")
                ));
            },
        );
    }

    #[test]
    fn sstore_sinc_sdec_reject_l1_slots_with_precompile_reverts() {
        let mut ctx = test_context();
        with_zone_provider(
            &mut ctx,
            MockL1Reader::default(),
            StorageActions::disabled(),
            |provider| {
                let write_actions = [
                    ZonePrecompileStorageProvider::sstore,
                    ZonePrecompileStorageProvider::sinc,
                    ZonePrecompileStorageProvider::sdec,
                ];
                let l1_slots = [
                    (PATH_USD_ADDRESS, tip20_slots::TRANSFER_POLICY_ID),
                    (TIP403_REGISTRY_ADDRESS, U256::ZERO),
                ];

                for action in write_actions {
                    for (address, key) in l1_slots {
                        let value = U256::ONE << (tip20_slots::TRANSFER_POLICY_ID_OFFSET * 8);
                        let err = action(provider, address, key, value).unwrap_err();
                        assert!(err.into_precompile_result(0, 0).unwrap().is_revert());
                    }
                }

                let local = (Address::with_last_byte(0x99), U256::from(88));
                for action in write_actions {
                    assert!(action(provider, local.0, local.1, U256::ONE).is_ok())
                }
            },
        );
    }

    #[test]
    fn sload_routes_registry_and_only_tip20_policy_field_to_l1() {
        let mut ctx = test_context();
        let l1 = MockL1Reader::default();
        let offset_bits = tip20_slots::TRANSFER_POLICY_ID_OFFSET * 8;
        let local_low_bits = U256::from(0xdead_u64);
        let local_policy = U256::from(1u64) << offset_bits;
        let l1_policy = U256::from(99u64) << offset_bits;
        l1.set_u256(
            PATH_USD_ADDRESS,
            tip20_slots::TRANSFER_POLICY_ID,
            123,
            l1_policy,
        );
        l1.set_u256(TIP403_REGISTRY_ADDRESS, U256::from(7), 123, U256::from(8));

        with_zone_provider(
            &mut ctx,
            l1.clone(),
            StorageActions::disabled(),
            |provider| {
                provider
                    .inner
                    .sstore(
                        PATH_USD_ADDRESS,
                        tip20_slots::TRANSFER_POLICY_ID,
                        local_low_bits | local_policy,
                    )
                    .unwrap();
                let overlaid = provider
                    .sload(PATH_USD_ADDRESS, tip20_slots::TRANSFER_POLICY_ID)
                    .unwrap();
                assert_eq!(overlaid & U256::from(0xffff_u64), local_low_bits);
                assert_eq!((overlaid >> offset_bits).to::<u64>(), 99);
                // Allow setNextQuoteToken's RMW when its policy bits match L1.
                provider
                    .sstore(
                        PATH_USD_ADDRESS,
                        tip20_slots::TRANSFER_POLICY_ID,
                        U256::from(0xbeef_u64) | l1_policy,
                    )
                    .unwrap();
                assert_eq!(
                    provider
                        .sload(TIP403_REGISTRY_ADDRESS, U256::from(7))
                        .unwrap(),
                    U256::from(8)
                );

                // The same packed slot at an ordinary address, and other slots at a TIP-20
                // address, remain entirely zone-local.
                let local_address = Address::with_last_byte(0x55);
                provider
                    .sstore(
                        local_address,
                        tip20_slots::TRANSFER_POLICY_ID,
                        U256::from(6),
                    )
                    .unwrap();
                assert_eq!(
                    provider
                        .sload(local_address, tip20_slots::TRANSFER_POLICY_ID)
                        .unwrap(),
                    U256::from(6)
                );
                let adjacent_tip20_slot = tip20_slots::TRANSFER_POLICY_ID + U256::ONE;
                provider
                    .sstore(PATH_USD_ADDRESS, adjacent_tip20_slot, U256::from(7))
                    .unwrap();
                assert_eq!(
                    provider
                        .sload(PATH_USD_ADDRESS, adjacent_tip20_slot)
                        .unwrap(),
                    U256::from(7)
                );
            },
        );
        assert!(l1.storage_requests().iter().all(|request| request.2 == 123));
        assert_eq!(l1.storage_requests().len(), 3);
        assert_eq!(l1.hardfork_requests(), vec![123]);
    }

    #[test]
    fn mirrored_reads_preserve_warming_gas_and_storage_actions() {
        let mut ctx = test_context();
        let l1 = MockL1Reader::default();
        let actions = StorageActions::enabled();
        l1.set_u256(TIP403_REGISTRY_ADDRESS, U256::ONE, 123, U256::from(5));

        with_zone_provider(&mut ctx, l1, actions.clone(), |provider| {
            let gas_before = provider.gas_used();
            assert_eq!(
                provider.sload(TIP403_REGISTRY_ADDRESS, U256::ONE).unwrap(),
                U256::from(5)
            );
            let cold_cost = provider.gas_used() - gas_before;
            let gas_before_warm = provider.gas_used();
            provider.sload(TIP403_REGISTRY_ADDRESS, U256::ONE).unwrap();
            let warm_cost = provider.gas_used() - gas_before_warm;
            assert!(
                cold_cost > warm_cost,
                "first local SLOAD must warm the mirrored slot"
            );
        });

        let recorded = actions.take().unwrap();
        assert!(recorded.ends_with(&[
            StorageAction::Sload(TIP403_REGISTRY_ADDRESS, U256::ONE, U256::ZERO),
            StorageAction::Sload(TIP403_REGISTRY_ADDRESS, U256::ONE, U256::ZERO),
        ]));
    }
}
