use alloy::{
    genesis::{ChainConfig, Genesis, GenesisAccount},
    primitives::{Address, Bytes, TxKind, U256, address},
    sol_types::SolValue,
};
use eyre::{WrapErr as _, eyre};
use reth_evm::{
    Evm as _, EvmEnv, EvmFactory,
    revm::{
        DatabaseCommit,
        context::{
            TxEnv,
            result::{ExecutionResult, Output},
        },
        database::{CacheDB, EmptyDB},
        state::AccountInfo,
    },
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tempo_chainspec::{hardfork::TempoHardfork, spec::TEMPO_BASE_FEE};
use tempo_evm::evm::{TempoEvm, TempoEvmFactory};
use tempo_revm::{TempoBlockEnv, TempoTxEnv};

const TEMPO_STATE_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000000");
const ZONE_INBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000001");
const ZONE_OUTBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000002");
const ZONE_CONFIG_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000003");

/// TempoStateReader precompile address — has no deployed contract code, but the zone EVM
/// registers a custom precompile here. We must insert dummy bytecode (`0xFE`) in genesis
/// so that Solidity's `EXTCODESIZE` check passes before issuing the STATICCALL.
const TEMPO_STATE_READER_ADDRESS: Address =
    address!("0x1c00000000000000000000000000000000000004");

const DEPLOYER: Address = address!("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

#[derive(Debug, clap::Parser)]
pub(crate) struct GenerateZoneGenesis {
    #[arg(short, long)]
    pub(crate) output: PathBuf,

    #[arg(long, default_value_t = 13371)]
    pub(crate) chain_id: u64,

    #[arg(long, default_value_t = TEMPO_BASE_FEE.into())]
    pub(crate) base_fee_per_gas: u128,

    #[arg(long, default_value_t = 30_000_000)]
    pub(crate) gas_limit: u64,

    #[arg(long)]
    pub(crate) zone_token: Address,

    #[arg(long)]
    pub(crate) tempo_portal: Address,

    #[arg(long)]
    pub(crate) tempo_genesis_header_rlp: String,

    #[arg(long)]
    pub(crate) sequencer: Option<Address>,

    #[arg(long, default_value = "docs/specs/out")]
    pub(crate) specs_out: PathBuf,
}

#[derive(serde::Deserialize)]
struct FoundryArtifact {
    bytecode: BytecodeField,
}

#[derive(serde::Deserialize)]
struct BytecodeField {
    object: String,
}

impl GenerateZoneGenesis {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let header_rlp = const_hex::decode(&self.tempo_genesis_header_rlp)
            .wrap_err("failed to decode hex string")?;

        let mut evm = setup_zone_evm(self.chain_id, self.gas_limit);

        evm.db_mut().insert_account_info(
            DEPLOYER,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000_000u128),
                ..Default::default()
            },
        );

        let tempo_state_bytecode = load_artifact(&self.specs_out, "TempoState")?;
        let tempo_state_args = (Bytes::from(header_rlp),).abi_encode_params();
        let mut nonce = 0u64;

        deploy_contract(
            &mut evm,
            &tempo_state_bytecode,
            &tempo_state_args,
            TEMPO_STATE_ADDRESS,
            "TempoState",
            self.chain_id,
            nonce,
        )?;
        nonce += 1;

        let zone_config_bytecode = load_artifact(&self.specs_out, "ZoneConfig")?;
        let zone_config_args =
            (self.zone_token, self.tempo_portal, TEMPO_STATE_ADDRESS).abi_encode_params();
        deploy_contract(
            &mut evm,
            &zone_config_bytecode,
            &zone_config_args,
            ZONE_CONFIG_ADDRESS,
            "ZoneConfig",
            self.chain_id,
            nonce,
        )?;
        nonce += 1;

        let zone_inbox_bytecode = load_artifact(&self.specs_out, "ZoneInbox")?;
        let zone_inbox_args = (
            ZONE_CONFIG_ADDRESS,
            self.tempo_portal,
            TEMPO_STATE_ADDRESS,
            self.zone_token,
        )
            .abi_encode_params();
        deploy_contract(
            &mut evm,
            &zone_inbox_bytecode,
            &zone_inbox_args,
            ZONE_INBOX_ADDRESS,
            "ZoneInbox",
            self.chain_id,
            nonce,
        )?;
        nonce += 1;

        let zone_outbox_bytecode = load_artifact(&self.specs_out, "ZoneOutbox")?;
        let zone_outbox_args = (ZONE_CONFIG_ADDRESS, self.zone_token).abi_encode_params();
        deploy_contract(
            &mut evm,
            &zone_outbox_bytecode,
            &zone_outbox_args,
            ZONE_OUTBOX_ADDRESS,
            "ZoneOutbox",
            self.chain_id,
            nonce,
        )?;

        // Insert dummy bytecode at the TempoStateReader precompile address.
        //
        // The zone EVM registers a custom precompile at this address, but Solidity ≥0.8
        // checks `EXTCODESIZE` before every high-level external call. If the address has
        // no code, the call reverts immediately without issuing the STATICCALL — the
        // precompile never gets a chance to execute. `0xFE` (INVALID opcode) is safe
        // because revm routes to the precompile before ever executing bytecode.
        {
            use reth_evm::revm::bytecode::Bytecode;
            evm.db_mut().insert_account_info(
                TEMPO_STATE_READER_ADDRESS,
                AccountInfo {
                    code: Some(Bytecode::new_raw(Bytes::from_static(&[0xFE]))),
                    nonce: 1,
                    ..Default::default()
                },
            );
            println!(
                "Inserted dummy bytecode at TempoStateReader precompile {TEMPO_STATE_READER_ADDRESS}"
            );
        }

        let db = evm.db_mut();
        for (name, addr) in [
            ("TempoState", TEMPO_STATE_ADDRESS),
            ("ZoneConfig", ZONE_CONFIG_ADDRESS),
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
            .filter(|(addr, _)| **addr != DEPLOYER)
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

        if let Some(sequencer) = self.sequencer {
            genesis_alloc.entry(sequencer).or_default().balance =
                U256::from(1_000_000_000_000_000_000_000u128);
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

        let json =
            serde_json::to_string_pretty(&genesis).wrap_err("failed encoding genesis as JSON")?;

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

fn load_artifact(specs_out: &Path, name: &str) -> eyre::Result<Vec<u8>> {
    let path = specs_out
        .join(format!("{name}.sol"))
        .join(format!("{name}.json"));
    let content = std::fs::read_to_string(&path)
        .wrap_err_with(|| format!("failed to read artifact at `{}`", path.display()))?;
    let artifact: FoundryArtifact = serde_json::from_str(&content)
        .wrap_err_with(|| format!("failed to parse artifact at `{}`", path.display()))?;
    const_hex::decode(&artifact.bytecode.object).wrap_err("failed to decode bytecode hex")
}

fn deploy_contract(
    evm: &mut TempoEvm<CacheDB<EmptyDB>>,
    creation_bytecode: &[u8],
    constructor_args: &[u8],
    predeploy_addr: Address,
    name: &str,
    chain_id: u64,
    nonce: u64,
) -> eyre::Result<()> {
    let mut initcode = Vec::with_capacity(creation_bytecode.len() + constructor_args.len());
    initcode.extend_from_slice(creation_bytecode);
    initcode.extend_from_slice(constructor_args);

    let tx = TempoTxEnv {
        inner: TxEnv {
            caller: DEPLOYER,
            gas_price: 0,
            gas_limit: 30_000_000,
            kind: TxKind::Create,
            data: initcode.into(),
            chain_id: Some(chain_id),
            nonce,
            ..Default::default()
        },
        ..Default::default()
    };

    let result = evm
        .transact_raw(tx)
        .map_err(|e| eyre!("{name} deployment tx failed: {e:?}"))?;

    let created_addr = match &result.result {
        ExecutionResult::Success { output, .. } => match output {
            Output::Create(_, Some(addr)) => *addr,
            _ => return Err(eyre!("{name} deployment did not return a created address")),
        },
        ExecutionResult::Revert { output, .. } => {
            return Err(eyre!("{name} deployment reverted: {output}"));
        }
        ExecutionResult::Halt { reason, .. } => {
            return Err(eyre!("{name} deployment halted: {reason:?}"));
        }
    };

    evm.db_mut().commit(result.state);

    let db = evm.db_mut();
    if let Some(mut created_account) = db.cache.accounts.remove(&created_addr) {
        created_account.info.nonce = 1;
        db.cache.accounts.insert(predeploy_addr, created_account);
    } else {
        return Err(eyre!(
            "{name} deployed to {created_addr} but account not found in CacheDB"
        ));
    }

    println!("Deployed {name} at {predeploy_addr} (created at {created_addr})");
    Ok(())
}
