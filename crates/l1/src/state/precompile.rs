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

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};
use tracing::{debug, error, warn};
use zone_primitives::constants::{ZONE_CONFIG_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};

use super::provider::L1StateProvider;

alloy_sol_types::sol! {
    /// Read a single storage slot from a Tempo L1 contract at a specific block height.
    function readStorageAt(address account, bytes32 slot, uint64 blockNumber) external view returns (bytes32);

    /// Read multiple storage slots from a Tempo L1 contract at a specific block height.
    function readStorageBatchAt(address account, bytes32[] calldata slots, uint64 blockNumber) external view returns (bytes32[] memory);

    /// Returned when the precompile is invoked via `DELEGATECALL` instead of `CALL`.
    error DelegateCallNotAllowed();

    /// Returned when the caller is not a zone system contract.
    error Unauthorized();
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
/// Zone system contracts (ZoneInbox, ZoneOutbox, ZoneConfig) pass the `tempoBlockNumber` from the
/// TempoState contract after `finalizeTempo` has been called.
///
/// # Restrictions
///
/// - Only direct `CALL`s are accepted; `DELEGATECALL` reverts with [`DelegateCallNotAllowed`].
/// - Only ZoneInbox, ZoneOutbox, and ZoneConfig may call this precompile directly.
/// - The precompile is **view-only** — it never writes to EVM state.
/// - On cache miss the provider retries the RPC fetch indefinitely with backoff, stalling
///   block production until L1 connectivity is restored.
pub struct TempoStateReader;

impl TempoStateReader {
    /// Create a [`DynPrecompile`] that dispatches `readStorageAt` and
    /// `readStorageBatchAt` calls to the given [`L1StateProvider`].
    ///
    /// The returned precompile captures `provider` by move and can be registered in a
    /// [`PrecompilesMap`](alloy_evm::precompiles::PrecompilesMap) at the TempoStateReader
    /// predeploy address.
    pub fn create(provider: L1StateProvider) -> DynPrecompile {
        DynPrecompile::new_stateful(
            PrecompileId::Custom("TempoStateReader".into()),
            move |input| {
                if !input.is_direct_call() {
                    warn!(target: "zone::precompile", "TempoStateReader called via DELEGATECALL — rejecting");
                    return Ok(PrecompileOutput::revert(
                        0,
                        DelegateCallNotAllowed {}.abi_encode().into(),
                        input.reservoir,
                    ));
                }

                if !Self::is_allowed_system_caller(input.caller) {
                    warn!(
                        target: "zone::precompile",
                        caller = %input.caller,
                        "TempoStateReader called by non-system contract — rejecting"
                    );
                    return Ok(PrecompileOutput::revert(
                        0,
                        Unauthorized {}.abi_encode().into(),
                        input.reservoir,
                    ));
                }

                let data = input.data;
                if data.len() < 4 {
                    warn!(target: "zone::precompile", data_len = data.len(), "TempoStateReader called with insufficient data");
                    return Ok(PrecompileOutput::revert(0, Bytes::new(), input.reservoir));
                }

                let selector: [u8; 4] = data[..4].try_into().expect("len >= 4");

                let result = if selector == readStorageAtCall::SELECTOR {
                    debug!(target: "zone::precompile", "TempoStateReader: readStorageAt");
                    Self::handle_single_slot(&provider, data, input.reservoir)
                } else if selector == readStorageBatchAtCall::SELECTOR {
                    debug!(target: "zone::precompile", "TempoStateReader: readStorageBatchAt");
                    Self::handle_multi_slot(&provider, data, input.reservoir)
                } else {
                    warn!(target: "zone::precompile", selector = ?selector, "TempoStateReader: unknown selector");
                    Ok(PrecompileOutput::revert(0, Bytes::new(), input.reservoir))
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

    fn is_allowed_system_caller(caller: Address) -> bool {
        matches!(
            caller,
            ZONE_INBOX_ADDRESS | ZONE_OUTBOX_ADDRESS | ZONE_CONFIG_ADDRESS
        )
    }

    /// Handle a `readStorageAt(address, bytes32, uint64)` call.
    ///
    /// Decodes the ABI calldata, performs a synchronous lookup via the provider at the specified
    /// L1 block number (cache first, then RPC fallback), and returns the ABI-encoded `bytes32`
    /// value. Returns a hard [`PrecompileError`] if both the cache and RPC fallback fail.
    fn handle_single_slot(
        provider: &L1StateProvider,
        data: &[u8],
        reservoir: u64,
    ) -> PrecompileResult {
        let call = match readStorageAtCall::abi_decode(data) {
            Ok(call) => call,
            Err(_) => return Ok(PrecompileOutput::revert(0, Bytes::new(), reservoir)),
        };

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
        Ok(PrecompileOutput::new(gas, encoded.into(), reservoir))
    }

    /// Handle a `readStorageBatchAt(address, bytes32[], uint64)` call.
    ///
    /// Decodes the ABI calldata, performs a synchronous lookup for each slot at the specified
    /// L1 block number (cache first, then RPC fallback), and returns the ABI-encoded `bytes32[]`
    /// result. If **any** slot fails both cache and RPC lookup, the entire call fails with a
    /// hard [`PrecompileError`].
    fn handle_multi_slot(
        provider: &L1StateProvider,
        data: &[u8],
        reservoir: u64,
    ) -> PrecompileResult {
        let call = match readStorageBatchAtCall::abi_decode(data) {
            Ok(call) => call,
            Err(_) => return Ok(PrecompileOutput::revert(0, Bytes::new(), reservoir)),
        };

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
        Ok(PrecompileOutput::new(gas, encoded.into(), reservoir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;

    use alloy_evm::{
        EvmInternals,
        precompiles::{Precompile as AlloyEvmPrecompile, PrecompileInput},
    };
    use alloy_primitives::{B256, U256, address};
    use alloy_provider::{Provider, ProviderBuilder};
    use alloy_rpc_client::RpcClient;
    use alloy_sol_types::{SolCall, SolError};
    use alloy_transport::mock::Asserter;
    use revm::{
        Context,
        database::{CacheDB, EmptyDB},
        precompile::{PrecompileOutput, PrecompileResult},
    };
    use tempo_alloy::TempoNetwork;
    use zone_primitives::constants::TEMPO_STATE_READER_ADDRESS;

    use super::super::{L1StateCache, L1StateProviderConfig};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
    type TestContext = Context<
        revm::context::BlockEnv,
        revm::context::TxEnv,
        revm::context::CfgEnv,
        CacheDB<EmptyDB>,
    >;

    const BLOCK_NUMBER: u64 = 42;

    fn l1_account() -> Address {
        address!("0x0000000000000000000000000000000000001000")
    }

    fn outsider() -> Address {
        address!("0x0000000000000000000000000000000000009999")
    }

    fn slot_a() -> B256 {
        B256::with_last_byte(0x01)
    }

    fn slot_b() -> B256 {
        B256::with_last_byte(0x02)
    }

    fn value_a() -> B256 {
        B256::with_last_byte(0xaa)
    }

    fn value_b() -> B256 {
        B256::with_last_byte(0xbb)
    }

    fn single_slot_calldata() -> Bytes {
        readStorageAtCall {
            account: l1_account(),
            slot: slot_a(),
            blockNumber: BLOCK_NUMBER,
        }
        .abi_encode()
        .into()
    }

    fn batch_calldata() -> Bytes {
        readStorageBatchAtCall {
            account: l1_account(),
            slots: vec![slot_a(), slot_b()],
            blockNumber: BLOCK_NUMBER,
        }
        .abi_encode()
        .into()
    }

    struct PrecompileHarness {
        ctx: TestContext,
        precompile: DynPrecompile,
        _rpc_asserter: Asserter,
        _runtime: tokio::runtime::Runtime,
    }

    impl PrecompileHarness {
        fn new() -> TestResult<Self> {
            let cache = L1StateCache::new(HashSet::from([l1_account()]));
            {
                let mut cache = cache.write();
                cache.set(l1_account(), slot_a(), BLOCK_NUMBER, value_a());
                cache.set(l1_account(), slot_b(), BLOCK_NUMBER, value_b());
            }

            let rpc_asserter = Asserter::new();
            let rpc = ProviderBuilder::new_with_network::<TempoNetwork>()
                .connect_client(RpcClient::mocked(rpc_asserter.clone()))
                .erased();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let provider = L1StateProvider::new_raw(
                L1StateProviderConfig::default(),
                cache,
                rpc,
                runtime.handle().clone(),
            );

            Ok(Self {
                ctx: Context::new(
                    CacheDB::new(EmptyDB::new()),
                    revm::primitives::hardfork::SpecId::default(),
                ),
                precompile: TempoStateReader::create(provider),
                _rpc_asserter: rpc_asserter,
                _runtime: runtime,
            })
        }

        fn direct_call(&mut self, caller: Address, calldata: Bytes) -> PrecompileResult {
            self.call(caller, calldata, true)
        }

        fn delegate_call(&mut self, caller: Address, calldata: Bytes) -> PrecompileResult {
            self.call(caller, calldata, false)
        }

        fn call(
            &mut self,
            caller: Address,
            calldata: Bytes,
            is_direct_call: bool,
        ) -> PrecompileResult {
            let bytecode_address = if is_direct_call {
                TEMPO_STATE_READER_ADDRESS
            } else {
                address!("0x000000000000000000000000000000000000dead")
            };

            AlloyEvmPrecompile::call(
                &self.precompile,
                PrecompileInput {
                    data: &calldata,
                    caller,
                    internals: EvmInternals::from_context(&mut self.ctx),
                    gas: 100_000,
                    reservoir: 0,
                    value: U256::ZERO,
                    is_static: true,
                    target_address: TEMPO_STATE_READER_ADDRESS,
                    bytecode_address,
                },
            )
        }
    }

    fn assert_unauthorized(output: PrecompileOutput) {
        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from(Unauthorized {}.abi_encode()));
    }

    fn assert_delegatecall_rejected(output: PrecompileOutput) {
        assert!(output.is_revert());
        assert_eq!(
            output.bytes,
            Bytes::from(DelegateCallNotAllowed {}.abi_encode())
        );
    }

    #[test]
    fn non_system_direct_caller_cannot_read_single_slot() -> TestResult {
        let mut harness = PrecompileHarness::new()?;

        let output = harness.direct_call(outsider(), single_slot_calldata())?;

        assert_unauthorized(output);
        Ok(())
    }

    #[test]
    fn non_system_direct_caller_cannot_read_batch() -> TestResult {
        let mut harness = PrecompileHarness::new()?;

        let output = harness.direct_call(outsider(), batch_calldata())?;

        assert_unauthorized(output);
        Ok(())
    }

    #[test]
    fn system_callers_can_read_single_slot() -> TestResult {
        let mut harness = PrecompileHarness::new()?;

        for caller in [ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZONE_CONFIG_ADDRESS] {
            let output = harness.direct_call(caller, single_slot_calldata())?;

            assert!(!output.is_revert());
            let decoded = readStorageAtCall::abi_decode_returns(&output.bytes)?;
            assert_eq!(decoded, value_a());
        }

        Ok(())
    }

    #[test]
    fn system_callers_can_read_batch() -> TestResult {
        let mut harness = PrecompileHarness::new()?;

        for caller in [ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZONE_CONFIG_ADDRESS] {
            let output = harness.direct_call(caller, batch_calldata())?;

            assert!(!output.is_revert());
            let decoded = readStorageBatchAtCall::abi_decode_returns(&output.bytes)?;
            assert_eq!(decoded, vec![value_a(), value_b()]);
        }

        Ok(())
    }

    #[test]
    fn delegatecall_rejection_is_preserved() -> TestResult {
        let mut harness = PrecompileHarness::new()?;

        let output = harness.delegate_call(outsider(), single_slot_calldata())?;

        assert_delegatecall_rejected(output);
        Ok(())
    }
}
