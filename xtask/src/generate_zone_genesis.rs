use alloy::{
    genesis::{ChainConfig, Genesis, GenesisAccount},
    primitives::{Address, Bytes, U256, address},
};
use eyre::{WrapErr as _, eyre};
use reth_evm::{
    Evm as _, EvmEnv, EvmFactory,
    revm::{
        DatabaseCommit,
        context::JournalTr,
        database::{CacheDB, EmptyDB},
        state::{AccountInfo, Bytecode},
    },
};
use std::{collections::BTreeMap, path::PathBuf};
use tempo_chainspec::{hardfork::TempoHardfork, spec::TEMPO_T0_BASE_FEE};
use tempo_contracts::{
    ARACHNID_CREATE2_FACTORY_ADDRESS, CREATEX_ADDRESS, MULTICALL3_ADDRESS, PERMIT2_ADDRESS,
    PERMIT2_SALT, SAFE_DEPLOYER_ADDRESS,
    contracts::{ARACHNID_CREATE2_FACTORY_BYTECODE, CreateX, Multicall3, SafeDeployer},
};
use tempo_evm::evm::{TempoEvm, TempoEvmFactory};
use tempo_precompiles::{
    PATH_USD_ADDRESS, TIP20_FACTORY_ADDRESS,
    account_keychain::AccountKeychain,
    nonce::NonceManager,
    receive_policy_guard::ReceivePolicyGuard,
    stablecoin_dex::StablecoinDEX,
    storage::{StorageActions, StorageCtx},
    storage_credits::StorageCredits,
    tip20::{ISSUER_ROLE, ITIP20, TIP20Token},
    tip20_factory::TIP20Factory,
    tip403_registry::TIP403Registry,
};
use tempo_primitives::TempoHeader;
use tempo_revm::TempoBlockEnv;
use zone_precompiles::{
    TempoState as NativeTempoState, ZoneFeeManager, ZoneInbox as NativeZoneInbox,
    ZoneOutbox as NativeZoneOutbox,
};

const TEMPO_STATE_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000000");
const ZONE_INBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000001");
const ZONE_OUTBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000002");

const DEPLOYER: Address = address!("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

#[derive(Debug, clap::Parser)]
pub(crate) struct GenerateZoneGenesis {
    #[arg(short, long)]
    pub(crate) output: PathBuf,

    #[arg(long)]
    pub(crate) chain_id: u64,

    #[arg(long, default_value_t = TEMPO_T0_BASE_FEE.into())]
    pub(crate) base_fee_per_gas: u128,

    #[arg(long, default_value_t = 30_000_000)]
    pub(crate) gas_limit: u64,

    /// Canonical fee token used when a zone transaction omits `fee_token`.
    #[arg(long, default_value_t = PATH_USD_ADDRESS)]
    pub(crate) default_fee_token: Address,

    /// RLP-encoded Tempo genesis header. Defaults to `TempoHeader::default()`.
    #[arg(long)]
    pub(crate) tempo_genesis_header_rlp: Option<String>,

    #[arg(long)]
    pub(crate) admin: Address,

    #[arg(long)]
    pub(crate) sequencer: Option<Address>,

    /// Include CreateX factory in genesis.
    #[arg(long)]
    pub(crate) with_createx: bool,

    /// Include Safe Singleton Factory in genesis.
    #[arg(long)]
    pub(crate) with_safe_deployer: bool,

    /// Include Arachnid CREATE2 factory in genesis.
    /// The factory is always used internally to deploy Permit2; this flag
    /// controls whether it remains in the final genesis state.
    #[arg(long)]
    pub(crate) with_create2_factory: bool,
}

impl GenerateZoneGenesis {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        if self.admin == Address::ZERO {
            return Err(eyre!("--admin must not be the zero address"));
        }

        let header_rlp = match &self.tempo_genesis_header_rlp {
            Some(header_rlp) => {
                const_hex::decode(header_rlp).wrap_err("failed to decode hex string")?
            }
            None => alloy_rlp::encode(TempoHeader::default()),
        };

        let mut evm = setup_zone_evm(self.chain_id, self.gas_limit);

        evm.db_mut().insert_account_info(
            DEPLOYER,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000_000u128),
                ..Default::default()
            },
        );

        // Initialize all precompiles and deploy standard contracts to match the
        // L1 genesis setup. The zone EVM uses the same TempoEvmFactory, so all
        // precompiles must be initialized for user transactions to work correctly.
        deploy_arachnid_create2_factory(&mut evm);
        deploy_permit2(&mut evm)?;

        initialize_tip403_registry(&mut evm)?;
        create_path_usd_token(&mut evm)?;
        initialize_fee_manager(&mut evm, self.default_fee_token)?;
        initialize_stablecoin_dex(&mut evm)?;
        initialize_nonce_manager(&mut evm)?;
        initialize_account_keychain(&mut evm)?;
        initialize_receive_policy_guard(&mut evm)?;
        initialize_storage_credits(&mut evm)?;

        initialize_tempo_state(&mut evm, &header_rlp)?;
        initialize_zone_inbox(&mut evm)?;
        initialize_zone_outbox(&mut evm)?;

        let native_state = evm.ctx_mut().journaled_state.finalize();
        evm.db_mut().commit(native_state);

        let db = evm.db_mut();
        for (name, addr) in [
            ("TempoState", TEMPO_STATE_ADDRESS),
            ("ZoneInbox", ZONE_INBOX_ADDRESS),
            ("ZoneOutbox", ZONE_OUTBOX_ADDRESS),
        ] {
            let account = db
                .cache
                .accounts
                .get(&addr)
                .ok_or_else(|| eyre!("{name} not found at {addr}"))?;
            let has_code = account.info.code.as_ref().is_some_and(|c| !c.is_empty());
            if !has_code {
                return Err(eyre!("{name} has no code at {addr}"));
            }
        }

        let mut genesis_alloc: BTreeMap<Address, GenesisAccount> = db
            .cache
            .accounts
            .iter()
            .filter(|(addr, _)| **addr != DEPLOYER && **addr != TIP20_FACTORY_ADDRESS)
            .filter(|(addr, _)| {
                self.with_create2_factory || **addr != ARACHNID_CREATE2_FACTORY_ADDRESS
            })
            .map(|(address, account)| {
                let storage: Option<BTreeMap<_, _>> = if !account.storage.is_empty() {
                    Some(
                        account
                            .storage
                            .iter()
                            .map(|(key, val)| ((*key).into(), (*val).into()))
                            .collect(),
                    )
                } else {
                    None
                };
                let genesis_account = GenesisAccount {
                    nonce: Some(account.info.nonce),
                    code: account.info.code.as_ref().map(|c| c.original_bytes()),
                    storage,
                    ..Default::default()
                };
                (*address, genesis_account)
            })
            .collect();

        // Include Address::ZERO in genesis so it exists in the state trie.
        // System transactions use this address as the sender, and TIP-20 burn
        // transfers to it. Without a trie entry, the parallel state root task
        // (sparse trie) can diverge when this account is touched and then
        // cleared under EIP-161 state-clear rules.
        genesis_alloc.entry(Address::ZERO).or_default().nonce = Some(1);

        // Deploy standard utility contracts matching L1 genesis.
        genesis_alloc.insert(
            MULTICALL3_ADDRESS,
            GenesisAccount {
                code: Some(Bytes::from_static(&Multicall3::DEPLOYED_BYTECODE)),
                nonce: Some(1),
                ..Default::default()
            },
        );
        if self.with_createx {
            genesis_alloc.insert(
                CREATEX_ADDRESS,
                GenesisAccount {
                    code: Some(Bytes::from_static(&CreateX::DEPLOYED_BYTECODE)),
                    nonce: Some(1),
                    ..Default::default()
                },
            );
        }
        if self.with_safe_deployer {
            genesis_alloc.insert(
                SAFE_DEPLOYER_ADDRESS,
                GenesisAccount {
                    code: Some(Bytes::from_static(&SafeDeployer::DEPLOYED_BYTECODE)),
                    nonce: Some(1),
                    ..Default::default()
                },
            );
        }

        let chain_config = ChainConfig {
            chain_id: self.chain_id,
            homestead_block: Some(0),
            eip150_block: Some(0),
            eip155_block: Some(0),
            eip158_block: Some(0),
            byzantium_block: Some(0),
            constantinople_block: Some(0),
            petersburg_block: Some(0),
            istanbul_block: Some(0),
            berlin_block: Some(0),
            london_block: Some(0),
            merge_netsplit_block: Some(0),
            shanghai_time: Some(0),
            cancun_time: Some(0),
            prague_time: Some(0),
            osaka_time: Some(0),
            terminal_total_difficulty: Some(U256::from(0)),
            terminal_total_difficulty_passed: true,
            deposit_contract_address: Some(Address::ZERO),
            ..Default::default()
        };

        let mut genesis = Genesis::default()
            .with_gas_limit(self.gas_limit)
            .with_base_fee(Some(self.base_fee_per_gas))
            .with_nonce(0x42)
            .with_extra_data(Bytes::from_static(b"tempo-zone-genesis"));

        genesis.alloc = genesis_alloc;
        genesis.config = chain_config;

        let genesis_json =
            serde_json::to_value(&genesis).wrap_err("failed encoding genesis as JSON")?;
        let mut json = serde_json::to_string_pretty(&genesis_json)
            .wrap_err("failed encoding genesis as JSON")?;
        json.push('\n');

        std::fs::create_dir_all(&self.output).wrap_err_with(|| {
            format!(
                "failed to create directory and parents for `{}`",
                self.output.display()
            )
        })?;
        let genesis_dst = self.output.join("genesis.json");
        std::fs::write(&genesis_dst, json).wrap_err_with(|| {
            format!("failed writing genesis to file `{}`", genesis_dst.display())
        })?;

        println!("Zone genesis written to {}", genesis_dst.display());

        Ok(())
    }
}

fn setup_zone_evm(chain_id: u64, gas_limit: u64) -> TempoEvm<CacheDB<EmptyDB>> {
    let db = CacheDB::default();
    let mut env: EvmEnv<TempoHardfork, TempoBlockEnv> =
        EvmEnv::default().with_timestamp(U256::ZERO);
    env.cfg_env.chain_id = chain_id;
    env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
    env.block_env.inner.gas_limit = gas_limit;

    let factory = TempoEvmFactory::default();
    factory.create_evm(db, env)
}

/// Deploys the Arachnid CREATE2 factory by directly inserting it into the EVM state.
fn deploy_arachnid_create2_factory(evm: &mut TempoEvm<CacheDB<EmptyDB>>) {
    println!("Deploying Arachnid CREATE2 factory at {ARACHNID_CREATE2_FACTORY_ADDRESS}");
    evm.db_mut().insert_account_info(
        ARACHNID_CREATE2_FACTORY_ADDRESS,
        AccountInfo {
            code: Some(Bytecode::new_raw(ARACHNID_CREATE2_FACTORY_BYTECODE)),
            nonce: 0,
            ..Default::default()
        },
    );
}

/// Deploys Permit2 contract via the Arachnid CREATE2 factory.
fn deploy_permit2(evm: &mut TempoEvm<CacheDB<EmptyDB>>) -> eyre::Result<()> {
    let bytecode = &tempo_contracts::Permit2::BYTECODE;
    let calldata: Bytes = PERMIT2_SALT
        .as_slice()
        .iter()
        .chain(bytecode.iter())
        .copied()
        .collect();

    println!("Deploying Permit2 via CREATE2 to {PERMIT2_ADDRESS}");
    let result =
        evm.transact_system_call(Address::ZERO, ARACHNID_CREATE2_FACTORY_ADDRESS, calldata)?;
    if !result.result.is_success() {
        return Err(eyre!("Permit2 deployment failed: {:?}", result));
    }
    evm.db_mut().commit(result.state);
    println!("Permit2 deployed successfully at {PERMIT2_ADDRESS}");
    Ok(())
}

/// Initialize the native TempoState precompile storage from the L1 genesis header.
fn initialize_tempo_state(
    evm: &mut TempoEvm<CacheDB<EmptyDB>>,
    header_rlp: &[u8],
) -> eyre::Result<()> {
    let ctx = evm.ctx_mut();
    StorageCtx::enter_evm(
        &mut ctx.journaled_state,
        &ctx.block,
        &ctx.cfg,
        &ctx.tx,
        StorageActions::disabled(),
        || NativeTempoState::new().initialize(header_rlp),
    )?;
    println!("Initialized native TempoState at {TEMPO_STATE_ADDRESS}");
    Ok(())
}

/// Initialize the native ZoneInbox account marker and storage.
fn initialize_zone_inbox(evm: &mut TempoEvm<CacheDB<EmptyDB>>) -> eyre::Result<()> {
    let ctx = evm.ctx_mut();
    StorageCtx::enter_evm(
        &mut ctx.journaled_state,
        &ctx.block,
        &ctx.cfg,
        &ctx.tx,
        StorageActions::disabled(),
        || NativeZoneInbox::new().initialize(),
    )?;
    println!("Initialized native ZoneInbox at {ZONE_INBOX_ADDRESS}");
    Ok(())
}

/// Initialize the native ZoneOutbox account marker and storage.
fn initialize_zone_outbox(evm: &mut TempoEvm<CacheDB<EmptyDB>>) -> eyre::Result<()> {
    let ctx = evm.ctx_mut();
    StorageCtx::enter_evm(
        &mut ctx.journaled_state,
        &ctx.block,
        &ctx.cfg,
        &ctx.tx,
        StorageActions::disabled(),
        || NativeZoneOutbox::new().initialize(),
    )?;
    println!("Initialized native ZoneOutbox at {ZONE_OUTBOX_ADDRESS}");
    Ok(())
}

/// Initialize the TIP403Registry precompile (required for fee token transfer checks).
fn initialize_tip403_registry(evm: &mut TempoEvm<CacheDB<EmptyDB>>) -> eyre::Result<()> {
    let ctx = evm.ctx_mut();
    StorageCtx::enter_evm(
        &mut ctx.journaled_state,
        &ctx.block,
        &ctx.cfg,
        &ctx.tx,
        StorageActions::disabled(),
        || TIP403Registry::new().initialize(),
    )?;
    println!("Initialized TIP403Registry");
    Ok(())
}

/// Create pathUSD as the default fee token at its reserved TIP20 address.
///
/// This mirrors the L1 genesis setup: the Tempo EVM handler defaults to pathUSD
/// (`0x20C0...`) as the fee token and validates its `currency == "USD"` storage.
/// Without this, user transactions on the zone revert with `InvalidFeeToken`.
/// ZoneInbox is the fixed token admin; the configured zone admin receives no token roles.
fn create_path_usd_token(evm: &mut TempoEvm<CacheDB<EmptyDB>>) -> eyre::Result<()> {
    let ctx = evm.ctx_mut();
    StorageCtx::enter_evm(
        &mut ctx.journaled_state,
        &ctx.block,
        &ctx.cfg,
        &ctx.tx,
        StorageActions::disabled(),
        || {
            TIP20Factory::new().create_token_reserved_address(
                PATH_USD_ADDRESS,
                "pathUSD",
                "pathUSD",
                "USD",
                Address::ZERO,
                ZONE_INBOX_ADDRESS,
            )?;

            let mut token = TIP20Token::from_address(PATH_USD_ADDRESS)?;
            // Allow address(0) to mint (system transactions use sender=0)
            token.grant_role_internal(Address::ZERO, *ISSUER_ROLE)?;
            // Grant ISSUER_ROLE to ZoneInbox so it can mint pathUSD on deposits
            token.grant_role_internal(ZONE_INBOX_ADDRESS, *ISSUER_ROLE)?;
            // Grant ISSUER_ROLE to ZoneOutbox so it can burn pathUSD on withdrawals
            token.grant_role_internal(ZONE_OUTBOX_ADDRESS, *ISSUER_ROLE)?;

            // Set a large supply cap
            token.set_supply_cap(
                ZONE_INBOX_ADDRESS,
                ITIP20::setSupplyCapCall {
                    newSupplyCap: U256::from(u128::MAX),
                },
            )?;

            Ok::<(), tempo_precompiles::error::TempoPrecompileError>(())
        },
    )?;

    println!("Created pathUSD fee token at {PATH_USD_ADDRESS}");
    Ok(())
}

/// Initialize the Zone fee manager precompile.
fn initialize_fee_manager(
    evm: &mut TempoEvm<CacheDB<EmptyDB>>,
    default_fee_token: Address,
) -> eyre::Result<()> {
    let ctx = evm.ctx_mut();
    StorageCtx::enter_evm(
        &mut ctx.journaled_state,
        &ctx.block,
        &ctx.cfg,
        &ctx.tx,
        StorageActions::disabled(),
        || {
            let mut fee_manager = ZoneFeeManager::new();
            fee_manager
                .initialize(default_fee_token)
                .expect("Could not init fee manager");
        },
    );
    println!("Initialized ZoneFeeManager with default fee token {default_fee_token}");
    Ok(())
}

/// Initialize the StablecoinDEX precompile.
fn initialize_stablecoin_dex(evm: &mut TempoEvm<CacheDB<EmptyDB>>) -> eyre::Result<()> {
    let ctx = evm.ctx_mut();
    StorageCtx::enter_evm(
        &mut ctx.journaled_state,
        &ctx.block,
        &ctx.cfg,
        &ctx.tx,
        StorageActions::disabled(),
        || StablecoinDEX::new().initialize(),
    )?;
    println!("Initialized StablecoinDEX");
    Ok(())
}

/// Initialize the NonceManager precompile.
fn initialize_nonce_manager(evm: &mut TempoEvm<CacheDB<EmptyDB>>) -> eyre::Result<()> {
    let ctx = evm.ctx_mut();
    StorageCtx::enter_evm(
        &mut ctx.journaled_state,
        &ctx.block,
        &ctx.cfg,
        &ctx.tx,
        StorageActions::disabled(),
        || NonceManager::new().initialize(),
    )?;
    println!("Initialized NonceManager");
    Ok(())
}

/// Initialize the AccountKeychain precompile.
fn initialize_account_keychain(evm: &mut TempoEvm<CacheDB<EmptyDB>>) -> eyre::Result<()> {
    let ctx = evm.ctx_mut();
    StorageCtx::enter_evm(
        &mut ctx.journaled_state,
        &ctx.block,
        &ctx.cfg,
        &ctx.tx,
        StorageActions::disabled(),
        || AccountKeychain::new().initialize(),
    )?;
    println!("Initialized AccountKeychain");
    Ok(())
}

/// Initialize the ReceivePolicyGuard precompile account.
fn initialize_receive_policy_guard(evm: &mut TempoEvm<CacheDB<EmptyDB>>) -> eyre::Result<()> {
    let ctx = evm.ctx_mut();
    StorageCtx::enter_evm(
        &mut ctx.journaled_state,
        &ctx.block,
        &ctx.cfg,
        &ctx.tx,
        StorageActions::disabled(),
        || ReceivePolicyGuard::new().initialize(),
    )?;
    println!("Initialized ReceivePolicyGuard");
    Ok(())
}

/// Initialize the StorageCredits precompile account.
///
/// TIP-1060 bookkeeping writes this account from the EVM handler, even when no transaction calls
/// the precompile directly. Keeping the account non-empty prevents EIP-161 from dropping the
/// sequential transition while the sparse-trie state hook still observes its storage updates.
fn initialize_storage_credits(evm: &mut TempoEvm<CacheDB<EmptyDB>>) -> eyre::Result<()> {
    let ctx = evm.ctx_mut();
    StorageCtx::enter_evm(
        &mut ctx.journaled_state,
        &ctx.block,
        &ctx.cfg,
        &ctx.tx,
        StorageActions::disabled(),
        || StorageCredits::new().initialize(),
    )?;
    println!("Initialized StorageCredits");
    Ok(())
}
