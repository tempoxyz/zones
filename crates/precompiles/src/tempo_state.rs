//! Native `TempoState` precompile.
//!
//! Replaces the Solidity TempoState predeploy at `0x1c00...0000` while
//! preserving the same ABI.

use alloc::vec::Vec;

use alloy_consensus::BlockHeader;
use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rlp::Decodable as _;
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_precompiles::{
    DelegateCallNotAllowed, Precompile as TempoPrecompile, charge_input_cost, dispatch,
    storage::{Handler, StorageCtx, evm::EvmPrecompileStorageProvider},
    view,
};
use tempo_precompiles_macros::contract;
use tempo_primitives::TempoHeader;
use tempo_zone_contracts::TempoState as TempoStateAbi;
use zone_primitives::constants::{
    TEMPO_STATE_ADDRESS, ZONE_CONFIG_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS,
};

alloy_sol_types::sol! {
    error Error(string);
    error StaticCallNotAllowed();
}

/// L1 storage access needed by `readTempoStorageSlot(s)`.
pub trait L1StorageReader: Clone + Send + Sync + 'static {
    /// Read `account[slot]` at `block_number` on Tempo L1.
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
    ) -> Result<B256, PrecompileError>;
}

#[contract(addr = TEMPO_STATE_ADDRESS)]
struct TempoStateStorage {
    tempo_block_hash: B256,
    general_gas_limit: u64,
    shared_gas_limit: u64,
    tempo_parent_hash: B256,
    tempo_beneficiary: Address,
    tempo_state_root: B256,
    tempo_transactions_root: B256,
    tempo_receipts_root: B256,
    tempo_block_number: u64,
    tempo_gas_limit: u64,
    tempo_gas_used: u64,
    tempo_timestamp: u64,
    tempo_timestamp_millis: u64,
    tempo_prev_randao: B256,
}

/// Native TempoState precompile.
pub struct TempoState<P> {
    provider: P,
    state: TempoStateStorage,
}

impl<P> TempoState<P> {
    /// Create a new native TempoState handler.
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            state: TempoStateStorage::new(),
        }
    }

    /// Initialize the predeploy account code and storage from the genesis Tempo header.
    pub fn initialize_genesis(header: &[u8]) -> tempo_precompiles::Result<()> {
        let mut state = TempoStateStorage::new();
        state.__initialize()?;
        Self::decode_and_store_header(&mut state, header)?;
        Ok(())
    }

    fn decode_header(header: &[u8]) -> Result<TempoHeader, ()> {
        let mut cursor = header;
        let decoded = TempoHeader::decode(&mut cursor).map_err(|_| ())?;
        if !cursor.is_empty() {
            return Err(());
        }
        Ok(decoded)
    }

    fn decode_and_store_header(
        state: &mut TempoStateStorage,
        header_rlp: &[u8],
    ) -> tempo_precompiles::Result<TempoHeader> {
        let header = Self::decode_header(header_rlp).map_err(|_| {
            tempo_precompiles::error::TempoPrecompileError::Fatal(
                "invalid Tempo genesis header RLP".into(),
            )
        })?;
        Self::store_header(state, header_rlp, &header)?;
        Ok(header)
    }

    fn store_header(
        state: &mut TempoStateStorage,
        header_rlp: &[u8],
        header: &TempoHeader,
    ) -> tempo_precompiles::Result<()> {
        state.tempo_block_hash.write(keccak256(header_rlp))?;
        state.general_gas_limit.write(header.general_gas_limit)?;
        state.shared_gas_limit.write(header.shared_gas_limit)?;
        state.tempo_parent_hash.write(header.parent_hash())?;
        state.tempo_beneficiary.write(header.beneficiary())?;
        state.tempo_state_root.write(header.state_root())?;
        state
            .tempo_transactions_root
            .write(header.transactions_root())?;
        state.tempo_receipts_root.write(header.receipts_root())?;
        state.tempo_block_number.write(header.number())?;
        state.tempo_gas_limit.write(header.gas_limit())?;
        state.tempo_gas_used.write(header.gas_used())?;
        state.tempo_timestamp.write(header.timestamp())?;
        state
            .tempo_timestamp_millis
            .write(header.timestamp_millis_part)?;
        state
            .tempo_prev_randao
            .write(header.mix_hash().unwrap_or_default())?;
        Ok(())
    }

    fn tempo_block_hash(&self) -> tempo_precompiles::Result<B256> {
        self.state.tempo_block_hash.read()
    }

    fn general_gas_limit(&self) -> tempo_precompiles::Result<u64> {
        self.state.general_gas_limit.read()
    }

    fn shared_gas_limit(&self) -> tempo_precompiles::Result<u64> {
        self.state.shared_gas_limit.read()
    }

    fn tempo_parent_hash(&self) -> tempo_precompiles::Result<B256> {
        self.state.tempo_parent_hash.read()
    }

    fn tempo_beneficiary(&self) -> tempo_precompiles::Result<Address> {
        self.state.tempo_beneficiary.read()
    }

    fn tempo_state_root(&self) -> tempo_precompiles::Result<B256> {
        self.state.tempo_state_root.read()
    }

    fn tempo_transactions_root(&self) -> tempo_precompiles::Result<B256> {
        self.state.tempo_transactions_root.read()
    }

    fn tempo_receipts_root(&self) -> tempo_precompiles::Result<B256> {
        self.state.tempo_receipts_root.read()
    }

    fn tempo_block_number(&self) -> tempo_precompiles::Result<u64> {
        self.state.tempo_block_number.read()
    }

    fn tempo_gas_limit(&self) -> tempo_precompiles::Result<u64> {
        self.state.tempo_gas_limit.read()
    }

    fn tempo_gas_used(&self) -> tempo_precompiles::Result<u64> {
        self.state.tempo_gas_used.read()
    }

    fn tempo_timestamp(&self) -> tempo_precompiles::Result<u64> {
        self.state.tempo_timestamp.read()
    }

    fn tempo_timestamp_millis(&self) -> tempo_precompiles::Result<u64> {
        self.state.tempo_timestamp_millis.read()
    }

    fn tempo_prev_randao(&self) -> tempo_precompiles::Result<B256> {
        self.state.tempo_prev_randao.read()
    }

    fn is_system_caller(caller: Address) -> bool {
        matches!(
            caller,
            ZONE_INBOX_ADDRESS | ZONE_OUTBOX_ADDRESS | ZONE_CONFIG_ADDRESS
        )
    }

    fn revert_error<E: SolError>(&self, error: E) -> PrecompileResult {
        Ok(self.state.storage.revert_output(error.abi_encode().into()))
    }

    fn revert_string(&self, message: &str) -> PrecompileResult {
        Ok(self
            .state
            .storage
            .revert_output(Error(message.into()).abi_encode().into()))
    }

    fn invalid_rlp(&self) -> PrecompileResult {
        self.revert_error(TempoStateAbi::InvalidRlpData {})
    }
}

impl<P: L1StorageReader> TempoState<P> {
    fn finalize_tempo(
        &mut self,
        sender: Address,
        call: TempoStateAbi::finalizeTempoCall,
    ) -> PrecompileResult {
        if self.state.storage.is_static() {
            return self.revert_error(StaticCallNotAllowed {});
        }
        if sender != ZONE_INBOX_ADDRESS {
            return self.revert_error(TempoStateAbi::OnlyZoneInbox {});
        }

        let prev_block_hash = match self.tempo_block_hash() {
            Ok(hash) => hash,
            Err(err) => return self.state.storage.error_result(err),
        };
        let prev_block_number = match self.tempo_block_number() {
            Ok(number) => number,
            Err(err) => return self.state.storage.error_result(err),
        };

        let header = match Self::decode_header(&call.header) {
            Ok(header) => header,
            Err(_) => return self.invalid_rlp(),
        };
        let tempo_block_hash = keccak256(&call.header);

        if header.parent_hash() != prev_block_hash {
            return self.revert_error(TempoStateAbi::InvalidParentHash {});
        }
        if header.number() != prev_block_number.saturating_add(1) {
            return self.revert_error(TempoStateAbi::InvalidBlockNumber {});
        }

        if let Err(err) = Self::store_header(&mut self.state, &call.header, &header) {
            return self.state.storage.error_result(err);
        }
        if let Err(err) = self.state.emit_event(TempoStateAbi::TempoBlockFinalized {
            blockHash: tempo_block_hash,
            blockNumber: header.number(),
            stateRoot: header.state_root(),
        }) {
            return self.state.storage.error_result(err);
        }

        Ok(self.state.storage.success_output(Bytes::new()))
    }

    fn read_tempo_storage_slot(
        &mut self,
        sender: Address,
        call: TempoStateAbi::readTempoStorageSlotCall,
    ) -> PrecompileResult {
        if !Self::is_system_caller(sender) {
            return self
                .revert_string("TempoState: only zone system contracts can read Tempo state");
        }

        let block_number = match self.tempo_block_number() {
            Ok(number) => number,
            Err(err) => return self.state.storage.error_result(err),
        };
        let value = self
            .provider
            .read_l1_storage(call.account, call.slot, block_number)?;
        Ok(self.state.storage.success_output(
            TempoStateAbi::readTempoStorageSlotCall::abi_encode_returns(&value).into(),
        ))
    }

    fn read_tempo_storage_slots(
        &mut self,
        sender: Address,
        call: TempoStateAbi::readTempoStorageSlotsCall,
    ) -> PrecompileResult {
        if !Self::is_system_caller(sender) {
            return self
                .revert_string("TempoState: only zone system contracts can read Tempo state");
        }

        let block_number = match self.tempo_block_number() {
            Ok(number) => number,
            Err(err) => return self.state.storage.error_result(err),
        };
        let mut values = Vec::with_capacity(call.slots.len());
        for slot in call.slots {
            values.push(
                self.provider
                    .read_l1_storage(call.account, slot, block_number)?,
            );
        }
        Ok(self.state.storage.success_output(
            TempoStateAbi::readTempoStorageSlotsCall::abi_encode_returns(&values).into(),
        ))
    }

    /// Wraps this precompile for registration in the zone EVM.
    pub fn create(
        provider: P,
        cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
    ) -> DynPrecompile {
        let spec = cfg.spec;
        let amsterdam_eip8037_enabled = cfg.enable_amsterdam_eip8037;
        let gas_params = cfg.gas_params.clone();

        DynPrecompile::new_stateful(PrecompileId::Custom("TempoState".into()), move |input| {
            if !input.is_direct_call() {
                return Ok(PrecompileOutput::revert(
                    0,
                    SolError::abi_encode(&DelegateCallNotAllowed {}).into(),
                    input.reservoir,
                ));
            }

            let mut storage = EvmPrecompileStorageProvider::new(
                input.internals,
                input.gas,
                input.reservoir,
                spec,
                amsterdam_eip8037_enabled,
                input.is_static,
                gas_params.clone(),
            );

            StorageCtx::enter(&mut storage, || {
                Self::new(provider.clone()).call(input.data, input.caller)
            })
        })
    }
}

impl<P: L1StorageReader> TempoPrecompile for TempoState<P> {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        if let Some(err) = charge_input_cost(&mut self.state.storage, calldata) {
            return err;
        }

        dispatch!(
            calldata,
            |call| match call {
                TempoStateAbi::TempoStateCalls {
                    tempoBlockHash(call) => view(call, |_| self.tempo_block_hash()),
                    tempoBlockNumber(call) => view(call, |_| self.tempo_block_number()),
                    tempoStateRoot(call) => view(call, |_| self.tempo_state_root()),
                    tempoParentHash(call) => view(call, |_| self.tempo_parent_hash()),
                    tempoBeneficiary(call) => view(call, |_| self.tempo_beneficiary()),
                    tempoTransactionsRoot(call) => view(call, |_| self.tempo_transactions_root()),
                    tempoReceiptsRoot(call) => view(call, |_| self.tempo_receipts_root()),
                    tempoGasLimit(call) => view(call, |_| self.tempo_gas_limit()),
                    tempoGasUsed(call) => view(call, |_| self.tempo_gas_used()),
                    tempoTimestamp(call) => view(call, |_| self.tempo_timestamp()),
                    tempoTimestampMillis(call) => view(call, |_| self.tempo_timestamp_millis()),
                    tempoPrevRandao(call) => view(call, |_| self.tempo_prev_randao()),
                    generalGasLimit(call) => view(call, |_| self.general_gas_limit()),
                    sharedGasLimit(call) => view(call, |_| self.shared_gas_limit()),
                    finalizeTempo(call) => self.finalize_tempo(msg_sender, call),
                    readTempoStorageSlot(call) => self.read_tempo_storage_slot(msg_sender, call),
                    readTempoStorageSlots(call) => self.read_tempo_storage_slots(msg_sender, call),
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_evm::{
        EvmInternals,
        precompiles::{DynPrecompile, Precompile as AlloyEvmPrecompile, PrecompileInput},
    };
    use alloy_primitives::{U256, address, b256};
    use alloy_rlp::Encodable as _;
    use alloy_sol_types::SolCall;
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
    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    #[derive(Clone)]
    struct MockL1Reader {
        value: B256,
    }

    impl L1StorageReader for MockL1Reader {
        fn read_l1_storage(
            &self,
            _account: Address,
            _slot: B256,
            _block_number: u64,
        ) -> Result<B256, PrecompileError> {
            Ok(self.value)
        }
    }

    fn encode_header(header: &TempoHeader) -> Bytes {
        let mut encoded = Vec::new();
        header.encode(&mut encoded);
        encoded.into()
    }

    fn test_context() -> TestContext {
        Context::new(CacheDB::new(EmptyDB::new()), TempoHardfork::default())
    }

    fn initialize(ctx: &mut TestContext, header: &[u8]) -> TestResult {
        let spec = ctx.cfg.spec;
        let amsterdam_eip8037_enabled = ctx.cfg.enable_amsterdam_eip8037;
        let gas_params = ctx.cfg.gas_params.clone();
        let mut storage = EvmPrecompileStorageProvider::new(
            EvmInternals::from_context(ctx),
            u64::MAX,
            0,
            spec,
            amsterdam_eip8037_enabled,
            false,
            gas_params,
        );

        StorageCtx::enter(&mut storage, || {
            TempoState::<MockL1Reader>::initialize_genesis(header)
        })?;
        Ok(())
    }

    fn call(
        ctx: &mut TestContext,
        precompile: &DynPrecompile,
        caller: Address,
        calldata: Bytes,
        is_static: bool,
    ) -> PrecompileResult {
        AlloyEvmPrecompile::call(
            precompile,
            PrecompileInput {
                data: &calldata,
                gas: u64::MAX,
                reservoir: 0,
                caller,
                value: U256::ZERO,
                target_address: TEMPO_STATE_ADDRESS,
                is_static,
                bytecode_address: TEMPO_STATE_ADDRESS,
                internals: EvmInternals::from_context(ctx),
            },
        )
    }

    #[test]
    fn finalize_tempo_updates_stored_header_fields() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child = TempoHeader {
            general_gas_limit: 12_000_000,
            shared_gas_limit: 4_000_000,
            timestamp_millis_part: 321,
            inner: alloy_consensus::Header {
                parent_hash: genesis_hash,
                beneficiary: address!("0x0000000000000000000000000000000000001234"),
                state_root: b256!(
                    "0x1111111111111111111111111111111111111111111111111111111111111111"
                ),
                transactions_root: b256!(
                    "0x2222222222222222222222222222222222222222222222222222222222222222"
                ),
                receipts_root: b256!(
                    "0x3333333333333333333333333333333333333333333333333333333333333333"
                ),
                number: 1,
                gas_limit: 30_000_000,
                gas_used: 7_000_000,
                timestamp: 99,
                mix_hash: b256!(
                    "0x4444444444444444444444444444444444444444444444444444444444444444"
                ),
                ..Default::default()
            },
            ..Default::default()
        };
        let child_rlp = encode_header(&child);
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());

        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            TempoStateAbi::finalizeTempoCall { header: child_rlp }
                .abi_encode()
                .into(),
            false,
        )?;
        assert!(output.is_success());

        let block_number = call(
            &mut ctx,
            &precompile,
            Address::ZERO,
            TempoStateAbi::tempoBlockNumberCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoBlockNumberCall::abi_decode_returns(&block_number.bytes)?,
            1
        );

        let gas_limit = call(
            &mut ctx,
            &precompile,
            Address::ZERO,
            TempoStateAbi::tempoGasLimitCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoGasLimitCall::abi_decode_returns(&gas_limit.bytes)?,
            30_000_000
        );

        let general = call(
            &mut ctx,
            &precompile,
            Address::ZERO,
            TempoStateAbi::generalGasLimitCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::generalGasLimitCall::abi_decode_returns(&general.bytes)?,
            12_000_000
        );

        Ok(())
    }

    #[test]
    fn read_tempo_storage_slot_is_system_only() -> TestResult {
        let genesis_rlp = encode_header(&TempoHeader::default());
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let expected = b256!("0xabababababababababababababababababababababababababababababababab");
        let precompile = TempoState::create(MockL1Reader { value: expected }, &ctx.cfg.clone());
        let calldata: Bytes = TempoStateAbi::readTempoStorageSlotCall {
            account: address!("0x0000000000000000000000000000000000009999"),
            slot: B256::ZERO,
        }
        .abi_encode()
        .into();

        let outsider = call(
            &mut ctx,
            &precompile,
            address!("0x000000000000000000000000000000000000aaaa"),
            calldata.clone(),
            true,
        )?;
        assert!(outsider.is_revert());

        let system = call(&mut ctx, &precompile, ZONE_CONFIG_ADDRESS, calldata, true)?;
        assert_eq!(
            TempoStateAbi::readTempoStorageSlotCall::abi_decode_returns(&system.bytes)?,
            expected
        );

        Ok(())
    }
}
