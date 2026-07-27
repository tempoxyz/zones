//! Native `TempoState` precompile.
//!
//! Replaces the Solidity TempoState predeploy at `0x1c00...0000` while
//! preserving the zone-facing checkpoint and Tempo storage read ABI.

use alloc::vec::Vec;

use crate::{
    ZoneResult,
    storage::{L1State, L1StorageReader},
};
use alloy_consensus::BlockHeader;
use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rlp::Decodable as _;
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::PrecompileResult;
use tempo_precompiles::{
    EncodePrecompileResult, charge_input_cost, dispatch, storage::Handler, view,
};
use tempo_precompiles_macros::contract;
use tempo_primitives::TempoHeader;
use tempo_zone_contracts::{TempoState as TempoStateAbi, TempoStateError};
use zone_primitives::constants::{
    PORTAL_ADMIN_SLOT, TEMPO_STATE_ADDRESS, ZONE_CONFIG_ADDRESS, ZONE_INBOX_ADDRESS,
    ZONE_OUTBOX_ADDRESS,
};

alloy_sol_types::sol! {
    error Error(string);
    error StaticCallNotAllowed();
}

#[contract(addr = TEMPO_STATE_ADDRESS)]
pub struct TempoState {
    tempo_block_hash: B256,
    pub(crate) tempo_block_number: u64,
}

/// Storage slot containing the finalized Tempo block number in Zone state.
pub const TEMPO_BLOCK_NUMBER_SLOT: alloy_primitives::U256 = slots::TEMPO_BLOCK_NUMBER;

/// Storage slot containing the finalized Tempo block hash in Zone state.
///
/// Zero means no checkpoint has been imported yet, which is distinct from a checkpoint at Tempo
/// block zero. Readers must consult this before treating [`TEMPO_BLOCK_NUMBER_SLOT`] as a height.
pub const TEMPO_BLOCK_HASH_SLOT: alloy_primitives::U256 = slots::TEMPO_BLOCK_HASH;

impl TempoState {
    /// Creates the direct-call-only `TempoState` precompile with checkpoint storage.
    ///
    /// System-only arbitrary L1 storage reads are delegated through `l1` at the stored checkpoint.
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

    /// Initializes the predeploy account code with an empty Tempo checkpoint.
    pub fn initialize(&mut self) -> tempo_precompiles::Result<()> {
        self.__initialize()
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
    pub(crate) fn finalize_checkpoint<P: L1StorageReader>(
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
        if prev_block_hash.is_zero() {
            let admin = l1.read_l1_storage(l1.portal(), PORTAL_ADMIN_SLOT, header.number())?;
            if admin.is_zero() {
                return Err(TempoStateError::portal_not_found().into());
            }
        } else {
            if header.parent_hash() != prev_block_hash {
                return Err(TempoStateError::invalid_parent_hash().into());
            }
            if prev_block_number.checked_add(1) != Some(header.number()) {
                return Err(TempoStateError::invalid_block_number().into());
            }
            l1.advance_anchor(prev_block_number, header.number())?;
        }

        let tempo_block_hash = self.write_checkpoint(&header_rlp, header.number())?;
        self.emit_event(TempoStateAbi::TempoBlockFinalized {
            blockHash: tempo_block_hash,
            blockNumber: header.number(),
            stateRoot: header.state_root(),
        })?;
        Ok(())
    }

    fn apply_checkpoint<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        sender: Address,
        call: TempoStateAbi::finalizeTempoCall,
    ) -> PrecompileResult {
        if self.storage.is_static() {
            return self.revert_error(StaticCallNotAllowed {});
        }
        if sender != ZONE_INBOX_ADDRESS {
            return self.revert_error(TempoStateAbi::OnlyZoneInbox {});
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

    /// Resolve the Tempo height that L1 reads must be anchored to.
    ///
    /// While the checkpoint is empty, `tempoBlockNumber` is a sentinel rather than a height.
    /// Reading L1 at zero would silently resolve against Tempo genesis, where this zone's portal
    /// does not exist yet, so callers fail closed instead.
    fn read_anchor(&mut self) -> tempo_precompiles::Result<Option<u64>> {
        if self.tempo_block_hash.read()?.is_zero() {
            return Ok(None);
        }
        self.tempo_block_number.read().map(Some)
    }

    fn read_tempo_storage_slot<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        sender: Address,
        call: TempoStateAbi::readTempoStorageSlotCall,
    ) -> PrecompileResult {
        if !Self::is_system_caller(sender) {
            return self
                .revert_string("TempoState: only zone system contracts can read Tempo state");
        }

        let block_number = match self.read_anchor() {
            Ok(Some(number)) => number,
            Ok(None) => return self.revert_error(TempoStateAbi::NoTempoCheckpoint {}),
            Err(err) => return self.storage.error_result(err),
        };
        let value = l1.read_l1_storage(call.account, call.slot, block_number)?;
        Ok(self.storage.success_output(
            TempoStateAbi::readTempoStorageSlotCall::abi_encode_returns(&value).into(),
        ))
    }

    fn read_tempo_storage_slots<P: L1StorageReader>(
        &mut self,
        l1: &L1State<P>,
        sender: Address,
        call: TempoStateAbi::readTempoStorageSlotsCall,
    ) -> PrecompileResult {
        if !Self::is_system_caller(sender) {
            return self
                .revert_string("TempoState: only zone system contracts can read Tempo state");
        }

        let block_number = match self.read_anchor() {
            Ok(Some(number)) => number,
            Ok(None) => return self.revert_error(TempoStateAbi::NoTempoCheckpoint {}),
            Err(err) => return self.storage.error_result(err),
        };
        let mut values = Vec::with_capacity(call.slots.len());
        for slot in call.slots {
            values.push(l1.read_l1_storage(call.account, slot, block_number)?);
        }
        Ok(self.storage.success_output(
            TempoStateAbi::readTempoStorageSlotsCall::abi_encode_returns(&values).into(),
        ))
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
                TempoStateAbi::TempoStateCalls {
                    tempoBlockHash(call) => view(call, |_| self.tempo_block_hash.read()),
                    tempoBlockNumber(call) => view(call, |_| self.tempo_block_number.read()),
                    finalizeTempo(call) => self.apply_checkpoint(l1, msg_sender, call),
                    readTempoStorageSlot(call) => {
                        self.read_tempo_storage_slot(l1, msg_sender, call)
                    },
                    readTempoStorageSlots(call) => {
                        self.read_tempo_storage_slots(l1, msg_sender, call)
                    },
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
    use alloy_evm::precompiles::DynPrecompile;
    use alloy_primitives::{U256, address, b256};
    use alloy_rlp::Encodable as _;
    use alloy_sol_types::SolCall;
    use tempo_precompiles::storage::StorageCtx;

    struct TempoStateHarness {
        ctx: TestContext,
        l1: L1State<MockL1Reader>,
        precompile: DynPrecompile,
    }

    impl TempoStateHarness {
        fn new(header: &TempoHeader) -> eyre::Result<Self> {
            Self::with_reader(header, MockL1Reader::default())
        }

        fn with_reader(header: &TempoHeader, reader: MockL1Reader) -> eyre::Result<Self> {
            let encoded = encode_header(header);
            Self::build(reader, Address::ZERO, |tempo_state| {
                tempo_state
                    .write_checkpoint(&encoded, header.number())
                    .map(|_| ())
            })
        }

        /// Harness whose `TempoState` still holds the empty genesis checkpoint.
        fn empty(reader: MockL1Reader, portal: Address) -> eyre::Result<Self> {
            Self::build(reader, portal, |_| Ok(()))
        }

        /// Initialize `TempoState`, apply `seed_checkpoint`, then wire up the precompile.
        fn build(
            reader: MockL1Reader,
            portal: Address,
            seed_checkpoint: impl FnOnce(&mut TempoState) -> tempo_precompiles::Result<()>,
        ) -> eyre::Result<Self> {
            let mut ctx = test_context();
            let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
            StorageCtx::enter(&mut storage, || {
                let mut tempo_state = TempoState::new();
                tempo_state.initialize()?;
                seed_checkpoint(&mut tempo_state)
            })?;
            drop(storage);

            let l1 = L1State::new(reader, portal);
            let precompile = TempoState::create(l1.clone(), &test_env(&ctx));
            Ok(Self {
                ctx,
                l1,
                precompile,
            })
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
            self.call(caller, finalize_calldata(header), is_static)
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
            let block_hash = self.call(
                Address::ZERO,
                TempoStateAbi::tempoBlockHashCall {}.abi_encode(),
                true,
            )?;
            assert_eq!(
                TempoStateAbi::tempoBlockHashCall::abi_decode_returns(&block_hash.bytes)?,
                expected_hash
            );

            let block_number = self.call(
                Address::ZERO,
                TempoStateAbi::tempoBlockNumberCall {}.abi_encode(),
                true,
            )?;
            assert_eq!(
                TempoStateAbi::tempoBlockNumberCall::abi_decode_returns(&block_number.bytes)?,
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

    fn finalize_calldata(header: Bytes) -> Bytes {
        TempoStateAbi::finalizeTempoCall { header }
            .abi_encode()
            .into()
    }

    fn read_slot_calldata() -> Bytes {
        TempoStateAbi::readTempoStorageSlotCall {
            account: Address::repeat_byte(0x44),
            slot: B256::ZERO,
        }
        .abi_encode()
        .into()
    }

    #[test]
    fn explicit_read_before_finalize_blocks_advancement() -> eyre::Result<()> {
        let genesis = child_header(B256::repeat_byte(0xaa), 10);
        let genesis_hash = keccak256(encode_header(&genesis));
        let mut harness = TempoStateHarness::with_reader(
            &genesis,
            MockL1Reader::returning(B256::repeat_byte(0x11)),
        )?;

        harness.call(ZONE_INBOX_ADDRESS, read_slot_calldata(), true)?;
        assert_eq!(harness.l1.get_anchor(), Some(10));

        let child = child_header(genesis_hash, 11);
        assert!(harness.finalize(ZONE_INBOX_ADDRESS, &child, false).is_err());
        Ok(())
    }

    #[test]
    fn explicit_read_after_finalize_uses_advanced_anchor() -> eyre::Result<()> {
        let genesis = child_header(B256::repeat_byte(0xaa), 10);
        let genesis_hash = keccak256(encode_header(&genesis));
        let reader = MockL1Reader::returning(B256::repeat_byte(0x11));
        let mut harness = TempoStateHarness::with_reader(&genesis, reader.clone())?;

        let child = child_header(genesis_hash, 11);
        harness.finalize(ZONE_INBOX_ADDRESS, &child, false)?;
        harness.call(ZONE_INBOX_ADDRESS, read_slot_calldata(), true)?;
        assert_eq!(harness.l1.get_anchor(), Some(11));
        assert!(
            reader
                .storage_requests()
                .iter()
                .all(|request| request.2 == 11)
        );
        Ok(())
    }

    #[test]
    fn initialize_starts_with_empty_checkpoint() -> eyre::Result<()> {
        let mut harness = TempoStateHarness::empty(MockL1Reader::default(), Address::ZERO)?;
        harness.assert_checkpoint(B256::ZERO, 0)
    }

    #[test]
    fn first_import_requires_initialized_portal() -> eyre::Result<()> {
        let reader = MockL1Reader::default();
        let portal = Address::repeat_byte(0x42);
        let header = child_header(B256::repeat_byte(0xaa), 42);
        let mut harness = TempoStateHarness::empty(reader.clone(), portal)?;

        let output = harness.finalize(ZONE_INBOX_ADDRESS, &header, false)?;
        assert!(output.is_revert());
        harness.assert_checkpoint(B256::ZERO, 0)?;
        assert_eq!(
            reader.storage_requests(),
            vec![(portal, PORTAL_ADMIN_SLOT, 42)]
        );
        Ok(())
    }

    #[test]
    fn first_import_accepts_any_block_after_portal_creation() -> eyre::Result<()> {
        let reader = MockL1Reader::default();
        let portal = Address::repeat_byte(0x42);
        reader.set_u256(
            portal,
            U256::from_be_bytes(PORTAL_ADMIN_SLOT.0),
            42,
            U256::from(1),
        );
        let header = child_header(B256::repeat_byte(0xaa), 42);
        let mut harness = TempoStateHarness::empty(reader.clone(), portal)?;

        let output = harness.finalize(ZONE_INBOX_ADDRESS, &header, false)?;
        assert!(output.is_success());
        harness.assert_checkpoint(keccak256(encode_header(&header)), 42)?;
        assert_eq!(harness.l1.get_anchor(), Some(42));
        assert_eq!(
            reader.storage_requests(),
            vec![(portal, PORTAL_ADMIN_SLOT, 42)]
        );
        Ok(())
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
            TempoStateAbi::tempoBlockHashCall {}.abi_encode(),
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

    #[test]
    fn read_tempo_storage_slot_is_system_only() -> eyre::Result<()> {
        let expected = b256!("0xabababababababababababababababababababababababababababababababab");
        let mut harness = TempoStateHarness::with_reader(
            &TempoHeader::default(),
            MockL1Reader::returning(expected),
        )?;
        let calldata: Bytes = TempoStateAbi::readTempoStorageSlotCall {
            account: address!("0x0000000000000000000000000000000000009999"),
            slot: B256::ZERO,
        }
        .abi_encode()
        .into();

        assert!(
            harness
                .call(
                    address!("0x000000000000000000000000000000000000aaaa"),
                    calldata.clone(),
                    true,
                )?
                .is_revert()
        );
        let system = harness.call(ZONE_CONFIG_ADDRESS, calldata, true)?;
        assert_eq!(
            TempoStateAbi::readTempoStorageSlotCall::abi_decode_returns(&system.bytes)?,
            expected
        );
        Ok(())
    }

    #[test]
    fn read_tempo_storage_slots_returns_batch() -> eyre::Result<()> {
        let expected = b256!("0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd");
        let mut harness = TempoStateHarness::with_reader(
            &TempoHeader::default(),
            MockL1Reader::returning(expected),
        )?;
        let output = harness.call(
            ZONE_OUTBOX_ADDRESS,
            TempoStateAbi::readTempoStorageSlotsCall {
                account: address!("0x0000000000000000000000000000000000009999"),
                slots: vec![B256::ZERO, B256::with_last_byte(1)],
            }
            .abi_encode(),
            true,
        )?;

        assert_eq!(
            TempoStateAbi::readTempoStorageSlotsCall::abi_decode_returns(&output.bytes)?,
            vec![expected, expected]
        );
        Ok(())
    }
}
