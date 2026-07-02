//! Native `TempoState` precompile.
//!
//! Replaces the Solidity TempoState predeploy at `0x1c00...0000` while
//! preserving the zone-facing ABI.

use alloc::vec::Vec;

use alloy_consensus::BlockHeader;
use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rlp::Decodable as _;
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_precompiles::{
    DelegateCallNotAllowed, charge_input_cost, dispatch,
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
pub struct TempoState {
    tempo_block_hash: B256,
    tempo_block_number: u64,
}

impl TempoState {
    /// Initialize the predeploy account code and storage from the genesis Tempo header.
    pub fn initialize_genesis(header: &[u8]) -> tempo_precompiles::Result<()> {
        let mut state = Self::new();
        state.__initialize()?;
        state.decode_and_store_checkpoint(header)?;
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

    fn decode_and_store_checkpoint(
        &mut self,
        header_rlp: &[u8],
    ) -> tempo_precompiles::Result<TempoHeader> {
        let header = Self::decode_header(header_rlp).map_err(|_| {
            tempo_precompiles::error::TempoPrecompileError::Fatal(
                "invalid Tempo genesis header RLP".into(),
            )
        })?;
        self.store_checkpoint(header_rlp, &header)?;
        Ok(header)
    }

    fn store_checkpoint(
        &mut self,
        header_rlp: &[u8],
        header: &TempoHeader,
    ) -> tempo_precompiles::Result<()> {
        self.tempo_block_hash.write(keccak256(header_rlp))?;
        self.tempo_block_number.write(header.number())?;
        Ok(())
    }

    fn tempo_block_hash(&self) -> tempo_precompiles::Result<B256> {
        self.tempo_block_hash.read()
    }

    fn tempo_block_number(&self) -> tempo_precompiles::Result<u64> {
        self.tempo_block_number.read()
    }

    fn is_system_caller(caller: Address) -> bool {
        matches!(
            caller,
            ZONE_INBOX_ADDRESS | ZONE_OUTBOX_ADDRESS | ZONE_CONFIG_ADDRESS
        )
    }

    fn revert_error<E: SolError>(&self, error: E) -> PrecompileResult {
        Ok(self.storage.revert_output(error.abi_encode().into()))
    }

    fn revert_string(&self, message: &str) -> PrecompileResult {
        Ok(self
            .storage
            .revert_output(Error(message.into()).abi_encode().into()))
    }

    fn invalid_rlp(&self) -> PrecompileResult {
        self.revert_error(TempoStateAbi::InvalidRlpData {})
    }

    fn finalize_tempo(
        &mut self,
        sender: Address,
        call: TempoStateAbi::finalizeTempoCall,
    ) -> PrecompileResult {
        if self.storage.is_static() {
            return self.revert_error(StaticCallNotAllowed {});
        }
        if sender != ZONE_INBOX_ADDRESS {
            return self.revert_error(TempoStateAbi::OnlyZoneInbox {});
        }

        let prev_block_hash = match self.tempo_block_hash() {
            Ok(hash) => hash,
            Err(err) => return self.storage.error_result(err),
        };
        let prev_block_number = match self.tempo_block_number() {
            Ok(number) => number,
            Err(err) => return self.storage.error_result(err),
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

        if let Err(err) = self.store_checkpoint(&call.header, &header) {
            return self.storage.error_result(err);
        }
        if let Err(err) = self.emit_event(TempoStateAbi::TempoBlockFinalized {
            blockHash: tempo_block_hash,
            blockNumber: header.number(),
            stateRoot: header.state_root(),
        }) {
            return self.storage.error_result(err);
        }

        Ok(self.storage.success_output(Bytes::new()))
    }

    fn read_tempo_storage_slot<P: L1StorageReader>(
        &mut self,
        provider: &P,
        sender: Address,
        call: TempoStateAbi::readTempoStorageSlotCall,
    ) -> PrecompileResult {
        if !Self::is_system_caller(sender) {
            return self
                .revert_string("TempoState: only zone system contracts can read Tempo state");
        }

        let block_number = match self.tempo_block_number() {
            Ok(number) => number,
            Err(err) => return self.storage.error_result(err),
        };
        let value = provider.read_l1_storage(call.account, call.slot, block_number)?;
        Ok(self.storage.success_output(
            TempoStateAbi::readTempoStorageSlotCall::abi_encode_returns(&value).into(),
        ))
    }

    fn read_tempo_storage_slots<P: L1StorageReader>(
        &mut self,
        provider: &P,
        sender: Address,
        call: TempoStateAbi::readTempoStorageSlotsCall,
    ) -> PrecompileResult {
        if !Self::is_system_caller(sender) {
            return self
                .revert_string("TempoState: only zone system contracts can read Tempo state");
        }

        let block_number = match self.tempo_block_number() {
            Ok(number) => number,
            Err(err) => return self.storage.error_result(err),
        };
        let mut values = Vec::with_capacity(call.slots.len());
        for slot in call.slots {
            values.push(provider.read_l1_storage(call.account, slot, block_number)?);
        }
        Ok(self.storage.success_output(
            TempoStateAbi::readTempoStorageSlotsCall::abi_encode_returns(&values).into(),
        ))
    }

    /// Wraps this precompile for registration in the zone EVM.
    pub fn create<P: L1StorageReader>(
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
                Self::new().call_with_provider(&provider, input.data, input.caller)
            })
        })
    }

    fn call_with_provider<P: L1StorageReader>(
        &mut self,
        provider: &P,
        calldata: &[u8],
        msg_sender: Address,
    ) -> PrecompileResult {
        if let Some(err) = charge_input_cost(&mut self.storage, calldata) {
            return err;
        }

        dispatch!(
            calldata,
            |call| match call {
                TempoStateAbi::TempoStateCalls {
                    tempoBlockHash(call) => view(call, |_| self.tempo_block_hash()),
                    tempoBlockNumber(call) => view(call, |_| self.tempo_block_number()),
                    finalizeTempo(call) => self.finalize_tempo(msg_sender, call),
                    readTempoStorageSlot(call) => {
                        self.read_tempo_storage_slot(provider, msg_sender, call)
                    },
                    readTempoStorageSlots(call) => {
                        self.read_tempo_storage_slots(provider, msg_sender, call)
                    },
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

        StorageCtx::enter(&mut storage, || TempoState::initialize_genesis(header))?;
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
    fn finalize_tempo_updates_checkpoint() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child = TempoHeader {
            inner: alloy_consensus::Header {
                parent_hash: genesis_hash,
                state_root: b256!(
                    "0x1111111111111111111111111111111111111111111111111111111111111111"
                ),
                number: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let child_rlp = encode_header(&child);
        let child_hash = keccak256(&child_rlp);
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

        let block_hash = call(
            &mut ctx,
            &precompile,
            Address::ZERO,
            TempoStateAbi::tempoBlockHashCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoBlockHashCall::abi_decode_returns(&block_hash.bytes)?,
            child_hash
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
