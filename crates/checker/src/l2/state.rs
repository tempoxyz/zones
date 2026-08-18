//! Exact L2 post-state observations for enabled TIP-20 tokens.

use alloy_eips::BlockNumHash;
use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, U256};
use eyre::WrapErr as _;
use reth_storage_api::{StateProvider, StateProviderFactory};
use tempo_precompiles::{
    storage::{ContractStorage, PrecompileStorageProvider, StorageCtx},
    tip20::{ISSUER_ROLE, ITIP20, TIP20Token},
};
use tempo_zone_contracts::{ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};
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
        unreachable!("read-only checker context")
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
        unreachable!("read-only checker context")
    }

    fn sstore(&mut self, _: Address, _: U256, _: U256) -> tempo_precompiles::error::Result<()> {
        unreachable!("read-only checker context")
    }

    fn tstore(&mut self, _: Address, _: U256, _: U256) -> tempo_precompiles::error::Result<()> {
        unreachable!("read-only checker context")
    }

    fn emit_event(
        &mut self,
        _: Address,
        _: alloy_primitives::LogData,
    ) -> tempo_precompiles::error::Result<()> {
        unreachable!("read-only checker context")
    }

    fn deduct_gas(&mut self, _: u64) -> tempo_precompiles::error::Result<()> {
        unreachable!("read-only checker context")
    }

    fn refund_gas(&mut self, _: i64) {
        unreachable!("read-only checker context")
    }

    fn gas_limit(&self) -> u64 {
        unreachable!("read-only checker context")
    }

    fn gas_used(&self) -> u64 {
        unreachable!("read-only checker context")
    }

    fn state_gas_used(&self) -> u64 {
        unreachable!("read-only checker context")
    }

    fn state_gas_spilled(&self) -> u64 {
        unreachable!("read-only checker context")
    }

    fn gas_refunded(&self) -> i64 {
        unreachable!("read-only checker context")
    }

    fn reservoir(&self) -> u64 {
        unreachable!("read-only checker context")
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
        unreachable!("read-only checker context")
    }

    fn checkpoint_commit(
        &mut self,
        _: revm::context_interface::journaled_state::JournalCheckpoint,
    ) {
        unreachable!("read-only checker context")
    }

    fn checkpoint_revert(
        &mut self,
        _: revm::context_interface::journaled_state::JournalCheckpoint,
    ) {
        unreachable!("read-only checker context")
    }

    fn set_tip1060_storage_credits(&mut self, _: bool) {}

    fn keccak256(&mut self, data: &[u8]) -> tempo_precompiles::error::Result<B256> {
        Ok(alloy_primitives::keccak256(data))
    }
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

/// Exact post-state observation for one token after the L2 block executed.
///
/// A successful state lookup returning no account becomes `Absent`, allowing
/// the model to report a state mismatch without treating it as an
/// infrastructure error. An actual provider/database failure remains an
/// extraction error.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum L2TokenStateEvidence {
    /// Token account exists at the exact post-state with observed metadata and roles.
    Present {
        token: Address,
        block: BlockNumHash,
        initialized: bool,
        name: String,
        symbol: String,
        currency: String,
        inbox_has_issuer_role: bool,
        outbox_has_issuer_role: bool,
    },
    /// Token account does not exist at the exact post-state.
    Absent { token: Address, block: BlockNumHash },
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

impl L2TokenStateEvidence {
    /// Return the token address regardless of presence.
    pub(crate) fn token(&self) -> Address {
        match self {
            Self::Present { token, .. } | Self::Absent { token, .. } => *token,
        }
    }
}

/// Read token observations from the exact post-state for an L2 block.
pub(super) fn read_token_enablement_state<P: StateProviderFactory>(
    provider: &P,
    tokens: &[Address],
    block: BlockNumHash,
) -> eyre::Result<Vec<L2TokenStateEvidence>> {
    let state = provider
        .state_by_block_hash(block.hash)
        .wrap_err_with(|| format!("failed to obtain state for block {}", block.hash))?;
    read_token_enablement_state_from(&*state, tokens, block)
}

fn read_token_enablement_state_from(
    state: &dyn StateProvider,
    tokens: &[Address],
    block: BlockNumHash,
) -> eyre::Result<Vec<L2TokenStateEvidence>> {
    // Force the exact-block provider to prove its backing state is readable.
    state
        .basic_account(&Address::ZERO)
        .wrap_err_with(|| format!("failed to read state for block {}", block.hash))?;

    let mut storage = ReadOnlyStorageProvider::new(state);
    StorageCtx::enter(&mut storage, || {
        tokens
            .iter()
            .map(|token| {
                let account_exists = state
                    .basic_account(token)
                    .wrap_err_with(|| {
                        format!(
                            "failed to read account for token {token} in block {}",
                            block.number
                        )
                    })?
                    .is_some();

                // A successful lookup returning no account is evidence of
                // absence — not an extraction error. The model can report a
                // state mismatch while the ExEx continues.
                if !account_exists {
                    return Ok(L2TokenStateEvidence::Absent {
                        token: *token,
                        block,
                    });
                }

                let tip20 = TIP20Token::from_address(*token).wrap_err_with(|| {
                    format!(
                        "token {token} is not a valid TIP-20 address in block {}",
                        block.number
                    )
                })?;
                let initialized = tip20.is_initialized().wrap_err_with(|| {
                    format!(
                        "failed to read initialization state for token {token} in block {}",
                        block.number
                    )
                })?;
                let name = tip20.name().wrap_err_with(|| {
                    format!(
                        "failed to read name for token {token} in block {}",
                        block.number
                    )
                })?;
                let symbol = tip20.symbol().wrap_err_with(|| {
                    format!(
                        "failed to read symbol for token {token} in block {}",
                        block.number
                    )
                })?;
                let currency = tip20.currency().wrap_err_with(|| {
                    format!(
                        "failed to read currency for token {token} in block {}",
                        block.number
                    )
                })?;
                let inbox_has_issuer_role = tip20
                    .has_role_internal(ZONE_INBOX_ADDRESS, *ISSUER_ROLE)
                    .wrap_err_with(|| {
                        format!(
                            "failed to read inbox issuer role for token {token} in block {}",
                            block.number
                        )
                    })?;
                let outbox_has_issuer_role = tip20
                    .has_role_internal(ZONE_OUTBOX_ADDRESS, *ISSUER_ROLE)
                    .wrap_err_with(|| {
                        format!(
                            "failed to read outbox issuer role for token {token} in block {}",
                            block.number
                        )
                    })?;

                Ok(L2TokenStateEvidence::Present {
                    token: *token,
                    block,
                    initialized,
                    name,
                    symbol,
                    currency,
                    inbox_has_issuer_role,
                    outbox_has_issuer_role,
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_storage_api::StateProviderFactory;

    const TOKEN: Address = address!("0x20C00000000000000000000000000000000000a1");

    fn short_string(value: &[u8]) -> U256 {
        let mut bytes = [0u8; 32];
        bytes[..value.len()].copy_from_slice(value);
        bytes[31] = (value.len() * 2) as u8;
        U256::from_be_bytes(bytes)
    }

    fn role_slot(account: Address, role: B256) -> B256 {
        let mut input = [0u8; 64];
        input[12..32].copy_from_slice(account.as_slice());
        let account_slot = alloy_primitives::keccak256(input);
        input[..32].copy_from_slice(role.as_slice());
        input[32..].copy_from_slice(account_slot.as_slice());
        alloy_primitives::keccak256(input)
    }

    fn provider_with_token() -> MockEthProvider {
        let issuer_role = alloy_primitives::keccak256(b"ISSUER_ROLE");
        let provider = MockEthProvider::default();
        provider.add_account(
            TOKEN,
            ExtendedAccount::new(1, U256::ZERO)
                .with_bytecode(vec![0xef].into())
                .extend_storage([
                    (
                        B256::from(U256::from(2).to_be_bytes::<32>()),
                        short_string(b"TestToken"),
                    ),
                    (
                        B256::from(U256::from(3).to_be_bytes::<32>()),
                        short_string(b"TT"),
                    ),
                    (
                        B256::from(U256::from(4).to_be_bytes::<32>()),
                        short_string(b"USD"),
                    ),
                    (role_slot(ZONE_INBOX_ADDRESS, issuer_role), U256::from(1)),
                    (role_slot(ZONE_OUTBOX_ADDRESS, issuer_role), U256::from(1)),
                ]),
        );
        provider
    }

    #[test]
    fn reads_metadata_and_roles() {
        let provider = provider_with_token();
        let state = provider.latest().unwrap();
        let observations = read_token_enablement_state_from(
            &*state,
            &[TOKEN],
            BlockNumHash::new(1, B256::repeat_byte(1)),
        )
        .unwrap();
        assert_eq!(observations.len(), 1);
        match &observations[0] {
            L2TokenStateEvidence::Present {
                initialized,
                name,
                symbol,
                currency,
                inbox_has_issuer_role,
                outbox_has_issuer_role,
                ..
            } => {
                assert!(*initialized);
                assert_eq!(name, "TestToken");
                assert_eq!(symbol, "TT");
                assert_eq!(currency, "USD");
                assert!(*inbox_has_issuer_role);
                assert!(*outbox_has_issuer_role);
            }
            L2TokenStateEvidence::Absent { .. } => panic!("expected Present"),
        }
    }

    #[test]
    fn missing_token_is_absent_not_error() {
        let provider = MockEthProvider::default();
        let state = provider.latest().unwrap();
        let observations = read_token_enablement_state_from(
            &*state,
            &[TOKEN],
            BlockNumHash::new(1, B256::repeat_byte(1)),
        )
        .unwrap();
        assert_eq!(observations.len(), 1);
        assert!(
            matches!(observations[0], L2TokenStateEvidence::Absent { token, .. } if token == TOKEN)
        );
    }

    #[test]
    fn existing_account_without_marker_is_uninitialized() {
        let provider = MockEthProvider::default();
        provider.add_account(TOKEN, ExtendedAccount::new(1, U256::ZERO));
        let state = provider.latest().unwrap();
        let observations = read_token_enablement_state_from(
            &*state,
            &[TOKEN],
            BlockNumHash::new(1, B256::repeat_byte(1)),
        )
        .unwrap();

        assert!(matches!(
            observations[0],
            L2TokenStateEvidence::Present {
                initialized: false,
                ..
            }
        ));
    }
}
