//! Native `TempoState` precompile.
//!
//! Replaces the Solidity TempoState predeploy at `0x1c00...0000` while
//! preserving the zone-facing checkpoint ABI.

use alloc::format;

use crate::{
    ZoneResult,
    storage::{L1State, L1StorageReader},
};
use alloy_consensus::BlockHeader;
use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rlp::Decodable as _;
use alloy_sol_types::SolError;
use revm::precompile::PrecompileResult;
use tempo_precompiles::{
    EncodePrecompileResult, charge_input_cost, dispatch, error::TempoPrecompileError,
    storage::Handler, view,
};
use tempo_precompiles_macros::contract;
use tempo_primitives::TempoHeader;
use tempo_zone_contracts::{ITempoState, TempoStateError};
use zone_primitives::constants::{TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS};

alloy_sol_types::sol! {
    error StaticCallNotAllowed();
}

#[contract(addr = TEMPO_STATE_ADDRESS)]
pub struct TempoState {
    tempo_block_hash: B256,
    pub(crate) tempo_block_number: u64,
}

/// Storage slot containing the finalized Tempo block number in Zone state.
pub const TEMPO_BLOCK_NUMBER_SLOT: alloy_primitives::U256 = slots::TEMPO_BLOCK_NUMBER;

impl TempoState {
    /// Creates the direct-call-only `TempoState` precompile with checkpoint storage.
    ///
    /// The shared L1 storage state is anchored by this precompile's finalized checkpoint.
    pub fn create<P: L1StorageReader>(
        l1: L1State<P>,
        env: &crate::ZonePrecompileEnv,
    ) -> DynPrecompile {
        crate::execution::create_precompile(
            "TempoState",
            env,
            crate::execution::NoCallRules,
            move |data, caller| Self::new().call_with_l1_state(&l1, data, caller),
        )
    }

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
        self.write_checkpoint(header_rlp, header.number())?;
        Ok(())
    }

    fn write_checkpoint(
        &mut self,
        header_rlp: &[u8],
        block_number: u64,
    ) -> tempo_precompiles::Result<B256> {
        let block_hash = keccak256(header_rlp);
        self.tempo_block_hash.write(block_hash)?;
        self.tempo_block_number.write(block_number)?;
        Ok(block_hash)
    }

    fn revert_error<E: SolError>(&self, error: E) -> PrecompileResult {
        Ok(self.storage.revert_output(error.abi_encode().into()))
    }

    /// Validate and apply a finalized Tempo checkpoint transition.
    ///
    /// IMPORTANT: this operation only enforces local continuity: the decoded block number must
    /// increment by one and its parent hash must match the previously stored Tempo hash.
    ///
    /// Canonicality is a separate proof obligation: the batch proof must bind the imported header
    /// hash and state root to the canonical settlement anchor and authenticate every Tempo storage
    /// read against that exact root.
    ///
    /// This typed operation is shared by the public `finalizeTempo` ABI and the native Inbox.
    pub(crate) fn finalize_checkpoint<P>(
        &mut self,
        l1: &L1State<P>,
        header_rlp: Bytes,
    ) -> ZoneResult<()> {
        let prev_block_hash = self.tempo_block_hash.read()?;
        let prev_block_number = self.tempo_block_number.read()?;

        let mut header_cursor = header_rlp.as_ref();
        let header = TempoHeader::decode(&mut header_cursor)
            .map_err(|_| TempoStateError::invalid_rlp_data())?;
        if !header_cursor.is_empty() {
            return Err(TempoStateError::invalid_rlp_data().into());
        }
        if header.parent_hash() != prev_block_hash {
            return Err(TempoStateError::invalid_parent_hash().into());
        }
        if prev_block_number.checked_add(1) != Some(header.number()) {
            return Err(TempoStateError::invalid_block_number().into());
        }

        l1.advance_anchor(prev_block_number, header.number())?;
        let tempo_block_hash = self.write_checkpoint(&header_rlp, header.number())?;
        self.emit_event(ITempoState::TempoBlockFinalized {
            blockHash: tempo_block_hash,
            blockNumber: header.number(),
            stateRoot: header.state_root(),
        })?;
        Ok(())
    }

    fn apply_checkpoint<P>(
        &mut self,
        l1: &L1State<P>,
        sender: Address,
        call: ITempoState::finalizeTempoCall,
    ) -> PrecompileResult {
        if self.storage.is_static() {
            return self.revert_error(StaticCallNotAllowed {});
        }
        if sender != ZONE_INBOX_ADDRESS {
            return self.revert_error(ITempoState::OnlyZoneInbox {});
        }

        self.finalize_checkpoint(l1, call.header)
            .encode_precompile_result(0, 0, |()| Bytes::new())
    }

    /// Returns the currently finalized Tempo block number from Zone state.
    pub(crate) fn tempo_block_number(&mut self) -> tempo_precompiles::Result<u64> {
        self.tempo_block_number.read()
    }

    /// Returns the currently finalized Tempo block hash from Zone state.
    pub(crate) fn tempo_block_hash(&mut self) -> tempo_precompiles::Result<B256> {
        self.tempo_block_hash.read()
    }

    /// Dispatch a `TempoState` call using execution-local L1 state.
    pub(crate) fn call_with_l1_state<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        calldata: &[u8],
        msg_sender: Address,
    ) -> PrecompileResult {
        if let Some(err) = charge_input_cost(&mut self.storage, calldata) {
            return err;
        }

        dispatch!(
            calldata,
            |call| match call {
                ITempoState::ITempoStateCalls {
                    tempoBlockHash(call) => view(call, |_| self.tempo_block_hash.read()),
                    tempoBlockNumber(call) => view(call, |_| self.tempo_block_number.read()),
                    finalizeTempo(call) => self.apply_checkpoint(l1, msg_sender, call),
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_utils::{
        MockL1Reader, TestContext, call_precompile, test_context, test_env, test_storage_provider,
    };
    use alloc::{vec, vec::Vec};
    use alloy_evm::precompiles::DynPrecompile;
    use alloy_primitives::{address, b256};
    use alloy_rlp::Encodable as _;
    use alloy_sol_types::SolCall;
    use tempo_precompiles::storage::StorageCtx;

    struct TempoStateHarness {
        ctx: TestContext,
        precompile: DynPrecompile,
    }

    impl TempoStateHarness {
        fn new(header: &TempoHeader) -> eyre::Result<Self> {
            let mut ctx = test_context();
            let encoded = encode_header(header);
            {
                let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
                StorageCtx::enter(&mut storage, || TempoState::new().initialize(&encoded))?;
            }
            let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
            let precompile = TempoState::create(l1, &test_env(&ctx));
            Ok(Self { ctx, precompile })
        }

        fn call(
            &mut self,
            caller: Address,
            calldata: impl Into<Bytes>,
            is_static: bool,
        ) -> PrecompileResult {
            self.call_as(
                caller,
                calldata,
                is_static,
                TEMPO_STATE_ADDRESS,
                TEMPO_STATE_ADDRESS,
            )
        }

        fn call_as(
            &mut self,
            caller: Address,
            calldata: impl Into<Bytes>,
            is_static: bool,
            target: Address,
            bytecode_address: Address,
        ) -> PrecompileResult {
            let calldata = calldata.into();
            call_precompile(
                &mut self.ctx,
                &self.precompile,
                caller,
                &calldata,
                u64::MAX,
                is_static,
                target,
                bytecode_address,
            )
        }

        fn finalize_raw(
            &mut self,
            caller: Address,
            header: Bytes,
            is_static: bool,
        ) -> PrecompileResult {
            let data = ITempoState::finalizeTempoCall { header }.abi_encode();
            self.call(caller, data, is_static)
        }

        fn finalize(
            &mut self,
            caller: Address,
            header: &TempoHeader,
            is_static: bool,
        ) -> PrecompileResult {
            self.finalize_raw(caller, encode_header(header), is_static)
        }

        fn assert_checkpoint(
            &mut self,
            expected_hash: B256,
            expected_number: u64,
        ) -> eyre::Result<()> {
            let hash_call = ITempoState::tempoBlockHashCall {};
            let block_hash = self.call(Address::ZERO, hash_call.abi_encode(), true)?;
            assert_eq!(
                ITempoState::tempoBlockHashCall::abi_decode_returns(&block_hash.bytes)?,
                expected_hash
            );

            let number_call = ITempoState::tempoBlockNumberCall {};
            let block_number = self.call(Address::ZERO, number_call.abi_encode(), true)?;
            assert_eq!(
                ITempoState::tempoBlockNumberCall::abi_decode_returns(&block_number.bytes)?,
                expected_number
            );
            Ok(())
        }
    }

    fn encode_header(header: &TempoHeader) -> Bytes {
        let mut encoded = Vec::new();
        header.encode(&mut encoded);
        encoded.into()
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

    #[test]
    fn initialize_sets_checkpoint() -> eyre::Result<()> {
        let header = child_header(B256::repeat_byte(0xaa), 42);
        let mut harness = TempoStateHarness::new(&header)?;
        harness.assert_checkpoint(keccak256(encode_header(&header)), 42)
    }

    #[test]
    fn finalize_tempo_updates_checkpoint() -> eyre::Result<()> {
        let genesis = TempoHeader::default();
        let genesis_hash = keccak256(encode_header(&genesis));
        let mut harness = TempoStateHarness::new(&genesis)?;
        let child = child_header(genesis_hash, 1);

        let output = harness.finalize(ZONE_INBOX_ADDRESS, &child, false)?;
        assert!(output.is_success());
        harness.assert_checkpoint(keccak256(encode_header(&child)), 1)
    }

    #[test]
    fn finalize_tempo_reverts_for_non_inbox_caller() -> eyre::Result<()> {
        let genesis = TempoHeader::default();
        let genesis_hash = keccak256(encode_header(&genesis));
        let mut harness = TempoStateHarness::new(&genesis)?;
        let child = child_header(genesis_hash, 1);

        assert!(harness.finalize(Address::ZERO, &child, false)?.is_revert());
        harness.assert_checkpoint(genesis_hash, genesis.number())
    }

    #[test]
    fn delegate_call_reverts() -> eyre::Result<()> {
        let mut harness = TempoStateHarness::new(&TempoHeader::default())?;
        let output = harness.call_as(
            Address::ZERO,
            ITempoState::tempoBlockHashCall {}.abi_encode(),
            true,
            TEMPO_STATE_ADDRESS,
            address!("0x000000000000000000000000000000000000dEaD"),
        )?;

        assert!(output.is_revert());
        assert_eq!(output.gas_used, 0);
        Ok(())
    }

    #[test]
    fn finalize_tempo_reverts_on_static_call() -> eyre::Result<()> {
        let genesis = TempoHeader::default();
        let genesis_hash = keccak256(encode_header(&genesis));
        let mut harness = TempoStateHarness::new(&genesis)?;
        let child = child_header(genesis_hash, 1);

        assert!(
            harness
                .finalize(ZONE_INBOX_ADDRESS, &child, true)?
                .is_revert()
        );
        harness.assert_checkpoint(genesis_hash, genesis.number())
    }

    #[test]
    fn finalize_tempo_reverts_on_invalid_rlp() -> eyre::Result<()> {
        let genesis = TempoHeader::default();
        let genesis_hash = keccak256(encode_header(&genesis));
        let mut harness = TempoStateHarness::new(&genesis)?;

        assert!(
            harness
                .finalize_raw(ZONE_INBOX_ADDRESS, Bytes::from(vec![0xff]), false)?
                .is_revert()
        );
        harness.assert_checkpoint(genesis_hash, genesis.number())
    }

    #[test]
    fn finalize_tempo_reverts_on_trailing_header_bytes() -> eyre::Result<()> {
        let genesis = TempoHeader::default();
        let genesis_hash = keccak256(encode_header(&genesis));
        let mut harness = TempoStateHarness::new(&genesis)?;
        let mut malformed = encode_header(&child_header(genesis_hash, 1)).to_vec();
        malformed.push(0);

        assert!(
            harness
                .finalize_raw(ZONE_INBOX_ADDRESS, Bytes::from(malformed), false)?
                .is_revert()
        );
        harness.assert_checkpoint(genesis_hash, genesis.number())
    }

    #[test]
    fn finalize_tempo_reverts_on_invalid_parent_hash() -> eyre::Result<()> {
        let genesis = TempoHeader::default();
        let genesis_hash = keccak256(encode_header(&genesis));
        let mut harness = TempoStateHarness::new(&genesis)?;
        let child = child_header(B256::ZERO, 1);

        assert!(
            harness
                .finalize(ZONE_INBOX_ADDRESS, &child, false)?
                .is_revert()
        );
        harness.assert_checkpoint(genesis_hash, genesis.number())
    }

    #[test]
    fn finalize_tempo_reverts_on_invalid_block_number() -> eyre::Result<()> {
        let genesis = TempoHeader::default();
        let genesis_hash = keccak256(encode_header(&genesis));
        let mut harness = TempoStateHarness::new(&genesis)?;
        let child = child_header(genesis_hash, 2);

        assert!(
            harness
                .finalize(ZONE_INBOX_ADDRESS, &child, false)?
                .is_revert()
        );
        harness.assert_checkpoint(genesis_hash, genesis.number())
    }
}
