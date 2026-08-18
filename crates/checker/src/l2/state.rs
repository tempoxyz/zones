//! Exact L2 state reads used by bootstrap and accounting verification.

use std::collections::{BTreeMap, BTreeSet};

use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, U256};
use eyre::WrapErr as _;
use reth_storage_api::{StateProvider, StateProviderFactory};
use tempo_precompiles::{
    storage::{ContractStorage, PrecompileStorageProvider, StorageCtx},
    tip20::{ITIP20, TIP20Token},
};
use zone_precompiles::zone_state::ZoneStateSnapshot;

/// Read-only bridge from Reth's exact-block state into Tempo's typed storage API.
struct ReadOnlyStorageProvider<'a> {
    state: &'a dyn StateProvider,
}

impl<'a> ReadOnlyStorageProvider<'a> {
    fn new(state: &'a dyn StateProvider) -> Self {
        Self { state }
    }
}

impl PrecompileStorageProvider for ReadOnlyStorageProvider<'_> {
    fn chain_id(&self) -> u64 {
        0
    }

    fn block_env(&self) -> &tempo_primitives::TempoBlockEnv {
        // These observations only use storage slots; the block environment is
        // present solely to satisfy the broader execution storage interface.
        use std::sync::OnceLock;
        static DEFAULT: OnceLock<tempo_primitives::TempoBlockEnv> = OnceLock::new();
        DEFAULT.get_or_init(tempo_primitives::TempoBlockEnv::default)
    }

    fn set_code(
        &mut self,
        _: Address,
        _: revm::state::Bytecode,
    ) -> tempo_precompiles::error::Result<()> {
        Err(read_only_error())
    }

    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&revm::state::AccountInfo),
    ) -> tempo_precompiles::error::Result<()> {
        let account = self.state.basic_account(&address).map_err(|error| {
            tempo_precompiles::error::TempoPrecompileError::Fatal(error.to_string())
        })?;
        f(&account.unwrap_or_default().into());
        Ok(())
    }

    fn account_code(
        &mut self,
        address: Address,
    ) -> tempo_precompiles::error::Result<(B256, revm::state::Bytecode)> {
        let Some(account) = self.state.basic_account(&address).map_err(|error| {
            tempo_precompiles::error::TempoPrecompileError::Fatal(error.to_string())
        })?
        else {
            return Ok(Default::default());
        };
        let code_hash = account.bytecode_hash.unwrap_or_default();
        let bytecode = if account.bytecode_hash.is_some() {
            self.state
                .bytecode_by_hash(&code_hash)
                .map_err(|error| {
                    tempo_precompiles::error::TempoPrecompileError::Fatal(error.to_string())
                })?
                .unwrap_or_default()
        } else {
            Default::default()
        };
        Ok((
            code_hash,
            revm::state::Bytecode::new_raw(bytecode.original_bytes()),
        ))
    }

    fn sload(&mut self, address: Address, key: U256) -> tempo_precompiles::error::Result<U256> {
        self.state
            .storage(address, B256::from(key.to_be_bytes::<32>()))
            .map(|value| value.unwrap_or_default())
            .map_err(|error| {
                tempo_precompiles::error::TempoPrecompileError::Fatal(error.to_string())
            })
    }

    fn tload(&mut self, _: Address, _: U256) -> tempo_precompiles::error::Result<U256> {
        Err(read_only_error())
    }

    fn sstore(&mut self, _: Address, _: U256, _: U256) -> tempo_precompiles::error::Result<()> {
        Err(read_only_error())
    }

    fn tstore(&mut self, _: Address, _: U256, _: U256) -> tempo_precompiles::error::Result<()> {
        Err(read_only_error())
    }

    fn emit_event(
        &mut self,
        _: Address,
        _: alloy_primitives::LogData,
    ) -> tempo_precompiles::error::Result<()> {
        Err(read_only_error())
    }

    fn deduct_gas(&mut self, _: u64) -> tempo_precompiles::error::Result<()> {
        Err(read_only_error())
    }

    fn refund_gas(&mut self, _: i64) {}

    fn gas_limit(&self) -> u64 {
        u64::MAX
    }

    fn gas_used(&self) -> u64 {
        0
    }

    fn state_gas_used(&self) -> u64 {
        0
    }

    fn state_gas_spilled(&self) -> u64 {
        0
    }

    fn gas_refunded(&self) -> i64 {
        0
    }

    fn reservoir(&self) -> u64 {
        0
    }

    fn spec(&self) -> tempo_chainspec::hardfork::TempoHardfork {
        tempo_chainspec::hardfork::TempoHardfork::T10
    }

    fn amsterdam_eip8037_enabled(&self) -> bool {
        false
    }

    fn is_static(&self) -> bool {
        true
    }

    fn checkpoint(&mut self) -> revm::context_interface::journaled_state::JournalCheckpoint {
        Default::default()
    }

    fn checkpoint_commit(
        &mut self,
        _: revm::context_interface::journaled_state::JournalCheckpoint,
    ) {
    }

    fn checkpoint_revert(
        &mut self,
        _: revm::context_interface::journaled_state::JournalCheckpoint,
    ) {
    }

    fn set_tip1060_storage_credits(&mut self, _: bool) {}

    fn keccak256(&mut self, data: &[u8]) -> tempo_precompiles::error::Result<B256> {
        Ok(alloy_primitives::keccak256(data))
    }
}

fn read_only_error() -> tempo_precompiles::error::TempoPrecompileError {
    tempo_precompiles::error::TempoPrecompileError::Fatal(
        "checker storage observation attempted to mutate state".into(),
    )
}

/// Protocol checkpoint embedded in local Zone genesis.
pub(crate) struct ZoneGenesisEvidence {
    pub(crate) tempo_block_hash: B256,
    pub(crate) tempo_block_number: u64,
    pub(crate) processed_deposit_queue_hash: B256,
    pub(crate) processed_deposit_number: u64,
    pub(crate) withdrawal_queue_hash: B256,
    pub(crate) withdrawal_batch_index: u64,
    pub(crate) default_fee_token: Address,
    pub(crate) initial_token_supply: U256,
}

/// Read the protocol checkpoint encoded in exact Zone state.
pub(crate) fn read_zone_genesis<P>(provider: &P, hash: B256) -> eyre::Result<ZoneGenesisEvidence>
where
    P: StateProviderFactory + ?Sized,
{
    let state = provider.state_by_block_hash(hash)?;
    let mut storage = ReadOnlyStorageProvider::new(&*state);
    StorageCtx::enter(&mut storage, || {
        let snapshot = ZoneStateSnapshot::read()?;
        let initial_token_supply =
            TIP20Token::from_address(snapshot.default_fee_token)?.total_supply()?;
        Ok(ZoneGenesisEvidence {
            tempo_block_hash: snapshot.tempo_block_hash,
            tempo_block_number: snapshot.tempo_block_number,
            processed_deposit_queue_hash: snapshot.processed_deposit_queue_hash,
            processed_deposit_number: snapshot.processed_deposit_number,
            withdrawal_queue_hash: snapshot.last_withdrawal_batch.withdrawalQueueHash,
            withdrawal_batch_index: snapshot.last_withdrawal_batch.withdrawalBatchIndex,
            default_fee_token: snapshot.default_fee_token,
            initial_token_supply,
        })
    })
}

/// Exact balances and supply for one token at one Zone block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenAccountingEvidence {
    pub(crate) token: Address,
    pub(crate) total_supply: U256,
    pub(crate) balances: BTreeMap<Address, U256>,
}

/// Read exact balances and supply for affected accounts at one Zone block.
pub(crate) fn read_accounting_state<P: StateProviderFactory>(
    provider: &P,
    accounts: &BTreeMap<Address, BTreeSet<Address>>,
    block: BlockNumHash,
) -> eyre::Result<Vec<TokenAccountingEvidence>> {
    let state = provider
        .state_by_block_hash(block.hash)
        .wrap_err_with(|| format!("failed to obtain state for block {}", block.hash))?;
    let mut storage = ReadOnlyStorageProvider::new(&*state);
    StorageCtx::enter(&mut storage, || {
        accounts
            .iter()
            .map(|(&token, accounts)| {
                let token = TIP20Token::from_address(token)?;
                let balances = accounts
                    .iter()
                    .map(|&account| {
                        token
                            .balance_of(ITIP20::balanceOfCall { account })
                            .map(|balance| (account, balance))
                    })
                    .collect::<tempo_precompiles::Result<BTreeMap<_, _>>>()?;
                Ok(TokenAccountingEvidence {
                    token: token.address(),
                    total_supply: token.total_supply()?,
                    balances,
                })
            })
            .collect::<tempo_precompiles::Result<Vec<_>>>()
    })
    .wrap_err_with(|| format!("failed to read accounting state for block {}", block.hash))
}
