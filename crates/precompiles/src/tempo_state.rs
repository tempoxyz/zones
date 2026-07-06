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
    error::TempoPrecompileError,
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

impl TempoState {
    /// Initializes the predeploy account code and checkpoint from the genesis Tempo header.
    pub fn initialize(&mut self, header_rlp: &[u8]) -> tempo_precompiles::Result<()> {
        self.__initialize()?;
        let mut cursor = header_rlp;
        let header = TempoHeader::decode(&mut cursor).map_err(|err| {
            TempoPrecompileError::Fatal(format!("invalid Tempo genesis header RLP: {err}"))
        })?;
        if !cursor.is_empty() {
            return Err(TempoPrecompileError::Fatal(
                "invalid Tempo genesis header RLP: trailing bytes after header".into(),
            ));
        }
        self.write_header(header_rlp, &header)?;
        Ok(())
    }

    fn write_header(
        &mut self,
        header_rlp: &[u8],
        header: &TempoHeader,
    ) -> tempo_precompiles::Result<B256> {
        let block_hash = keccak256(header_rlp);
        self.tempo_block_hash.write(block_hash)?;
        self.general_gas_limit.write(header.general_gas_limit)?;
        self.shared_gas_limit.write(header.shared_gas_limit)?;
        self.tempo_parent_hash.write(header.parent_hash())?;
        self.tempo_beneficiary.write(header.beneficiary())?;
        self.tempo_state_root.write(header.state_root())?;
        self.tempo_transactions_root
            .write(header.transactions_root())?;
        self.tempo_receipts_root.write(header.receipts_root())?;
        self.tempo_block_number.write(header.number())?;
        self.tempo_gas_limit.write(header.gas_limit())?;
        self.tempo_gas_used.write(header.gas_used())?;
        self.tempo_timestamp.write(header.timestamp())?;
        self.tempo_timestamp_millis
            .write(header.timestamp_millis_part)?;
        self.tempo_prev_randao
            .write(header.mix_hash().unwrap_or_default())?;
        Ok(block_hash)
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

    fn apply_checkpoint(
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

        let prev_block_hash = match self.tempo_block_hash.read() {
            Ok(hash) => hash,
            Err(err) => return self.storage.error_result(err),
        };
        let prev_block_number = match self.tempo_block_number.read() {
            Ok(number) => number,
            Err(err) => return self.storage.error_result(err),
        };

        let mut header_cursor = call.header.as_ref();
        let header = match TempoHeader::decode(&mut header_cursor) {
            Ok(header) => header,
            Err(_) => return self.revert_error(TempoStateAbi::InvalidRlpData {}),
        };
        if !header_cursor.is_empty() {
            return self.revert_error(TempoStateAbi::InvalidRlpData {});
        }

        if header.parent_hash() != prev_block_hash {
            return self.revert_error(TempoStateAbi::InvalidParentHash {});
        }
        if header.number() != prev_block_number.saturating_add(1) {
            return self.revert_error(TempoStateAbi::InvalidBlockNumber {});
        }

        let tempo_block_hash = match self.write_header(&call.header, &header) {
            Ok(hash) => hash,
            Err(err) => return self.storage.error_result(err),
        };
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

        let block_number = match self.tempo_block_number.read() {
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

        let block_number = match self.tempo_block_number.read() {
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
                    tempoBlockHash(call) => view(call, |_| self.tempo_block_hash.read()),
                    tempoBlockNumber(call) => view(call, |_| self.tempo_block_number.read()),
                    tempoStateRoot(call) => view(call, |_| self.tempo_state_root.read()),
                    tempoParentHash(call) => view(call, |_| self.tempo_parent_hash.read()),
                    tempoBeneficiary(call) => view(call, |_| self.tempo_beneficiary.read()),
                    tempoTransactionsRoot(call) => {
                        view(call, |_| self.tempo_transactions_root.read())
                    },
                    tempoReceiptsRoot(call) => view(call, |_| self.tempo_receipts_root.read()),
                    tempoGasLimit(call) => view(call, |_| self.tempo_gas_limit.read()),
                    tempoGasUsed(call) => view(call, |_| self.tempo_gas_used.read()),
                    tempoTimestamp(call) => view(call, |_| self.tempo_timestamp.read()),
                    tempoTimestampMillis(call) => {
                        view(call, |_| self.tempo_timestamp_millis.read())
                    },
                    tempoPrevRandao(call) => view(call, |_| self.tempo_prev_randao.read()),
                    generalGasLimit(call) => view(call, |_| self.general_gas_limit.read()),
                    sharedGasLimit(call) => view(call, |_| self.shared_gas_limit.read()),
                    finalizeTempo(call) => self.apply_checkpoint(msg_sender, call),
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

        StorageCtx::enter(&mut storage, || TempoState::new().initialize(header))?;
        Ok(())
    }

    fn call(
        ctx: &mut TestContext,
        precompile: &DynPrecompile,
        caller: Address,
        calldata: Bytes,
        is_static: bool,
    ) -> PrecompileResult {
        call_with_bytecode_address(
            ctx,
            precompile,
            caller,
            calldata,
            is_static,
            TEMPO_STATE_ADDRESS,
        )
    }

    fn call_with_bytecode_address(
        ctx: &mut TestContext,
        precompile: &DynPrecompile,
        caller: Address,
        calldata: Bytes,
        is_static: bool,
        bytecode_address: Address,
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
                bytecode_address,
                internals: EvmInternals::from_context(ctx),
            },
        )
    }

    fn child_header(parent_hash: B256, number: u64) -> TempoHeader {
        TempoHeader {
            general_gas_limit: 1_000_000,
            shared_gas_limit: 2_000_000,
            timestamp_millis_part: 123,
            inner: alloy_consensus::Header {
                parent_hash,
                beneficiary: address!("0x000000000000000000000000000000000000bEEF"),
                state_root: b256!(
                    "0x1111111111111111111111111111111111111111111111111111111111111111"
                ),
                transactions_root: b256!(
                    "0x2222222222222222222222222222222222222222222222222222222222222222"
                ),
                receipts_root: b256!(
                    "0x3333333333333333333333333333333333333333333333333333333333333333"
                ),
                number,
                gas_limit: 30_000_000,
                gas_used: 21_000,
                timestamp: 1_700_000_000,
                mix_hash: b256!(
                    "0x4444444444444444444444444444444444444444444444444444444444444444"
                ),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn finalize_calldata(header: Bytes) -> Bytes {
        TempoStateAbi::finalizeTempoCall { header }
            .abi_encode()
            .into()
    }

    fn assert_checkpoint(
        ctx: &mut TestContext,
        precompile: &DynPrecompile,
        expected_hash: B256,
        expected_number: u64,
    ) -> TestResult {
        let block_hash = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoBlockHashCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoBlockHashCall::abi_decode_returns(&block_hash.bytes)?,
            expected_hash
        );

        let block_number = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoBlockNumberCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoBlockNumberCall::abi_decode_returns(&block_number.bytes)?,
            expected_number
        );
        Ok(())
    }

    #[test]
    fn initialize_sets_header_fields() -> TestResult {
        let header = child_header(B256::repeat_byte(0xaa), 42);
        let header_rlp = encode_header(&header);
        let mut ctx = test_context();
        initialize(&mut ctx, &header_rlp)?;

        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        assert_checkpoint(&mut ctx, &precompile, keccak256(&header_rlp), 42)?;
        assert_legacy_getters(&mut ctx, &precompile, &header)?;

        Ok(())
    }

    #[test]
    fn finalize_tempo_updates_checkpoint() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child = child_header(genesis_hash, 1);
        let child_rlp = encode_header(&child);
        let child_hash = keccak256(&child_rlp);
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());

        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(child_rlp),
            false,
        )?;
        assert!(output.is_success());
        assert_checkpoint(&mut ctx, &precompile, child_hash, 1)?;
        assert_legacy_getters(&mut ctx, &precompile, &child)?;

        Ok(())
    }

    fn assert_legacy_getters(
        ctx: &mut TestContext,
        precompile: &DynPrecompile,
        header: &TempoHeader,
    ) -> TestResult {
        let state_root = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoStateRootCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoStateRootCall::abi_decode_returns(&state_root.bytes)?,
            header.state_root()
        );

        let parent = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoParentHashCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoParentHashCall::abi_decode_returns(&parent.bytes)?,
            header.parent_hash()
        );

        let beneficiary = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoBeneficiaryCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoBeneficiaryCall::abi_decode_returns(&beneficiary.bytes)?,
            header.beneficiary()
        );

        let transactions_root = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoTransactionsRootCall {}
                .abi_encode()
                .into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoTransactionsRootCall::abi_decode_returns(&transactions_root.bytes)?,
            header.transactions_root()
        );

        let receipts_root = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoReceiptsRootCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoReceiptsRootCall::abi_decode_returns(&receipts_root.bytes)?,
            header.receipts_root()
        );

        let gas_limit = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoGasLimitCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoGasLimitCall::abi_decode_returns(&gas_limit.bytes)?,
            header.gas_limit()
        );

        let gas_used = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoGasUsedCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoGasUsedCall::abi_decode_returns(&gas_used.bytes)?,
            header.gas_used()
        );

        let timestamp = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoTimestampCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoTimestampCall::abi_decode_returns(&timestamp.bytes)?,
            header.timestamp()
        );

        let timestamp_millis = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoTimestampMillisCall {}
                .abi_encode()
                .into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoTimestampMillisCall::abi_decode_returns(&timestamp_millis.bytes)?,
            header.timestamp_millis_part
        );

        let prev_randao = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::tempoPrevRandaoCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::tempoPrevRandaoCall::abi_decode_returns(&prev_randao.bytes)?,
            header.mix_hash().unwrap_or_default()
        );

        let general_gas_limit = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::generalGasLimitCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::generalGasLimitCall::abi_decode_returns(&general_gas_limit.bytes)?,
            header.general_gas_limit
        );

        let shared_gas_limit = call(
            ctx,
            precompile,
            Address::ZERO,
            TempoStateAbi::sharedGasLimitCall {}.abi_encode().into(),
            true,
        )?;
        assert_eq!(
            TempoStateAbi::sharedGasLimitCall::abi_decode_returns(&shared_gas_limit.bytes)?,
            header.shared_gas_limit
        );

        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_for_non_inbox_caller() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child_rlp = encode_header(&child_header(genesis_hash, 1));
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            Address::ZERO,
            finalize_calldata(child_rlp),
            false,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

        Ok(())
    }

    #[test]
    fn delegate_call_reverts() -> TestResult {
        let genesis_rlp = encode_header(&TempoHeader::default());
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call_with_bytecode_address(
            &mut ctx,
            &precompile,
            Address::ZERO,
            TempoStateAbi::tempoBlockHashCall {}.abi_encode().into(),
            true,
            address!("0x000000000000000000000000000000000000dEaD"),
        )?;

        assert!(output.is_revert());

        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_on_static_call() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child_rlp = encode_header(&child_header(genesis_hash, 1));
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(child_rlp),
            true,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_on_invalid_rlp() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(Bytes::from(vec![0xff])),
            false,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_on_trailing_header_bytes() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child_rlp = encode_header(&child_header(genesis_hash, 1));
        let mut malformed = child_rlp.to_vec();
        malformed.push(0);
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(Bytes::from(malformed)),
            false,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_on_invalid_parent_hash() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child_rlp = encode_header(&child_header(B256::ZERO, 1));
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(child_rlp),
            false,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_on_invalid_block_number() -> TestResult {
        let genesis = TempoHeader::default();
        let genesis_rlp = encode_header(&genesis);
        let genesis_hash = keccak256(&genesis_rlp);
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let child_rlp = encode_header(&child_header(genesis_hash, 2));
        let precompile = TempoState::create(MockL1Reader { value: B256::ZERO }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_INBOX_ADDRESS,
            finalize_calldata(child_rlp),
            false,
        )?;

        assert!(output.is_revert());
        assert_checkpoint(&mut ctx, &precompile, genesis_hash, genesis.number())?;

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

    #[test]
    fn read_tempo_storage_slots_returns_batch() -> TestResult {
        let genesis_rlp = encode_header(&TempoHeader::default());
        let mut ctx = test_context();
        initialize(&mut ctx, &genesis_rlp)?;

        let expected = b256!("0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd");
        let precompile = TempoState::create(MockL1Reader { value: expected }, &ctx.cfg.clone());
        let output = call(
            &mut ctx,
            &precompile,
            ZONE_OUTBOX_ADDRESS,
            TempoStateAbi::readTempoStorageSlotsCall {
                account: address!("0x0000000000000000000000000000000000009999"),
                slots: vec![B256::ZERO, B256::with_last_byte(1)],
            }
            .abi_encode()
            .into(),
            true,
        )?;

        assert_eq!(
            TempoStateAbi::readTempoStorageSlotsCall::abi_decode_returns(&output.bytes)?,
            vec![expected, expected]
        );

        Ok(())
    }
}
