//! Shared test utilities for zone contract deployment and EVM setup.
//!
//! Provides helpers to load Foundry artifacts, deploy zone predeploy contracts,
//! and build an in-memory EVM for integration testing against real contract bytecode.

use alloy_evm::{Evm, EvmEnv, EvmFactory};
use alloy_primitives::{Address, Bytes, U256, address};
use revm::{
    context::result::{ExecutionResult, Output},
    database::{CacheDB, EmptyDB},
    state::AccountInfo,
};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_evm::evm::{TempoEvm, TempoEvmFactory};
use tempo_revm::TempoBlockEnv;

pub const TEMPO_STATE_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000000");
pub const ZONE_INBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000001");
pub const ZONE_OUTBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000002");
pub const ZONE_CONFIG_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000003");
pub const TEMPO_STATE_READER_ADDRESS: Address =
    address!("0x1c00000000000000000000000000000000000004");

pub const DEPLOYER: Address = address!("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

/// Load a Foundry artifact's creation bytecode from the specs output directory.
///
/// Resolves the artifact path relative to `CARGO_MANIFEST_DIR`, walking up to the
/// workspace root and then into `docs/specs/out/<name>.sol/<name>.json`.
pub fn load_artifact(name: &str) -> Vec<u8> {
    load_artifact_from_root(name, None)
}

/// Load a Foundry artifact from a specified workspace root, or auto-detect it.
pub fn load_artifact_from_root(name: &str, workspace_root: Option<&std::path::Path>) -> Vec<u8> {
    #[derive(serde::Deserialize)]
    struct FoundryArtifact {
        bytecode: BytecodeField,
    }
    #[derive(serde::Deserialize)]
    struct BytecodeField {
        object: String,
    }

    let specs_out = if let Some(root) = workspace_root {
        root.join("docs/specs/out")
    } else {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        manifest_dir.join("../../docs/specs/out")
    };

    let path = specs_out
        .join(format!("{name}.sol"))
        .join(format!("{name}.json"));
    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let artifact: FoundryArtifact =
        serde_json::from_str(&content).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    const_hex::decode(&artifact.bytecode.object).expect("decode bytecode hex")
}

/// Deploy a contract into the CacheDB via the EVM, then relocate it to the predeploy address.
///
/// This mirrors the `xtask generate_zone_genesis` process: the contract is deployed at
/// a CREATE address, then the account is moved to the desired predeploy slot.
pub fn deploy_contract(
    evm: &mut TempoEvm<CacheDB<EmptyDB>>,
    creation_bytecode: &[u8],
    constructor_args: &[u8],
    predeploy_addr: Address,
    name: &str,
    chain_id: u64,
    nonce: u64,
) {
    use alloy_primitives::TxKind;
    use revm::context::TxEnv;
    use tempo_revm::TempoTxEnv;

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
        .unwrap_or_else(|e| panic!("{name} deploy tx failed: {e:?}"));
    let created_addr = match &result.result {
        ExecutionResult::Success { output, .. } => match output {
            Output::Create(_, Some(addr)) => *addr,
            other => panic!("{name} deploy did not return address: {other:?}"),
        },
        ExecutionResult::Revert { output, .. } => panic!("{name} deploy reverted: {output}"),
        ExecutionResult::Halt { reason, .. } => panic!("{name} deploy halted: {reason:?}"),
    };

    use revm::DatabaseCommit;
    evm.db_mut().commit(result.state);

    let db = evm.db_mut();
    if let Some(mut created_account) = db.cache.accounts.remove(&created_addr) {
        created_account.info.nonce = 1;
        db.cache.accounts.insert(predeploy_addr, created_account);
    } else {
        panic!("{name} deployed to {created_addr} but not found in CacheDB");
    }
}

/// Build a minimal valid RLP-encoded TempoHeader (genesis-like, all fields zeroed).
pub fn build_dummy_header_rlp() -> Vec<u8> {
    use alloy_rlp::Encodable;
    use tempo_primitives::TempoHeader;

    let header = TempoHeader::default();
    let mut buf = Vec::new();
    header.encode(&mut buf);
    buf
}

/// Build an EVM with the four zone predeploy contracts deployed in-memory.
///
/// This mirrors the genesis setup from `xtask generate_zone_genesis`:
/// 1. `TempoState` at `0x1c00..0000`
/// 2. `ZoneConfig` at `0x1c00..0003`
/// 3. `ZoneInbox` at `0x1c00..0001`
/// 4. `ZoneOutbox` at `0x1c00..0002`
///
/// The returned EVM uses a `CacheDB<EmptyDB>` with the deployer funded and all
/// contracts deployed. `chain_id` defaults to `13371`.
pub fn setup_zone_evm(chain_id: u64) -> TempoEvm<CacheDB<EmptyDB>> {
    let gas_limit = 30_000_000u64;

    let db = CacheDB::default();
    let mut env: EvmEnv<TempoHardfork, TempoBlockEnv> =
        EvmEnv::default().with_timestamp(U256::ZERO);
    env.cfg_env.chain_id = chain_id;
    env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
    env.block_env.inner.gas_limit = gas_limit;

    let factory = TempoEvmFactory::default();
    let mut evm = factory.create_evm(db, env);

    evm.db_mut().insert_account_info(
        DEPLOYER,
        AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000_000u128),
            ..Default::default()
        },
    );

    let dummy_header_rlp = build_dummy_header_rlp();

    let mut nonce = 0u64;

    // 1. TempoState(bytes headerRlp)
    let tempo_state_bytecode = load_artifact("TempoState");
    let tempo_state_args =
        alloy_sol_types::SolValue::abi_encode_params(&(Bytes::from(dummy_header_rlp.clone()),));
    deploy_contract(
        &mut evm,
        &tempo_state_bytecode,
        &tempo_state_args,
        TEMPO_STATE_ADDRESS,
        "TempoState",
        chain_id,
        nonce,
    );
    nonce += 1;

    // 2. ZoneConfig(address zoneToken, address tempoPortal, address tempoState)
    let zone_config_bytecode = load_artifact("ZoneConfig");
    let zone_token = Address::repeat_byte(0xaa);
    let tempo_portal = Address::repeat_byte(0xbb);
    let zone_config_args = alloy_sol_types::SolValue::abi_encode_params(&(
        zone_token,
        tempo_portal,
        TEMPO_STATE_ADDRESS,
    ));
    deploy_contract(
        &mut evm,
        &zone_config_bytecode,
        &zone_config_args,
        ZONE_CONFIG_ADDRESS,
        "ZoneConfig",
        chain_id,
        nonce,
    );
    nonce += 1;

    // 3. ZoneInbox(address config, address tempoPortal, address tempoState, address zoneToken)
    let zone_inbox_bytecode = load_artifact("ZoneInbox");
    let zone_inbox_args = alloy_sol_types::SolValue::abi_encode_params(&(
        ZONE_CONFIG_ADDRESS,
        tempo_portal,
        TEMPO_STATE_ADDRESS,
        zone_token,
    ));
    deploy_contract(
        &mut evm,
        &zone_inbox_bytecode,
        &zone_inbox_args,
        ZONE_INBOX_ADDRESS,
        "ZoneInbox",
        chain_id,
        nonce,
    );
    nonce += 1;

    // 4. ZoneOutbox(address config, address zoneToken)
    let zone_outbox_bytecode = load_artifact("ZoneOutbox");
    let zone_outbox_args =
        alloy_sol_types::SolValue::abi_encode_params(&(ZONE_CONFIG_ADDRESS, zone_token));
    deploy_contract(
        &mut evm,
        &zone_outbox_bytecode,
        &zone_outbox_args,
        ZONE_OUTBOX_ADDRESS,
        "ZoneOutbox",
        chain_id,
        nonce,
    );

    evm
}

/// Convenience wrapper that calls [`setup_zone_evm`] with the default chain ID (13371).
pub fn setup_zone_evm_default() -> TempoEvm<CacheDB<EmptyDB>> {
    setup_zone_evm(13371)
}

/// Snapshot of a single account extracted from a CacheDB.
#[derive(Debug, Clone)]
pub struct AccountSnapshot {
    pub nonce: u64,
    pub balance: U256,
    pub code_hash: alloy_primitives::B256,
    pub code: Option<Vec<u8>>,
    /// Non-zero storage slots.
    pub storage: Vec<(U256, U256)>,
}

/// Extract all accounts and their non-zero storage from a CacheDB.
///
/// Useful for building a `ZoneStateWitness` from a CacheDB that has contracts
/// deployed via [`setup_zone_evm`].
pub fn extract_db_accounts(db: &CacheDB<EmptyDB>) -> Vec<(Address, AccountSnapshot)> {
    let mut accounts = Vec::new();
    for (addr, account) in &db.cache.accounts {
        let info = &account.info;
        let code = info.code.as_ref().map(|c| c.bytes().to_vec());
        let storage: Vec<(U256, U256)> = account
            .storage
            .iter()
            .filter(|(_, v)| !v.is_zero())
            .map(|(k, v)| (*k, *v))
            .collect();
        accounts.push((
            *addr,
            AccountSnapshot {
                nonce: info.nonce,
                balance: info.balance,
                code_hash: info.code_hash,
                code,
                storage,
            },
        ));
    }
    accounts
}

/// Register a mock TempoStateReader precompile on a `TempoEvm`.
///
/// The mock returns `bytes32(0)` for `readStorageAt` and an array of zeros for
/// `readStorageBatchAt`. This is sufficient for `advanceTempo` calls in tests
/// where no real L1 data is needed (e.g., empty deposit queues).
pub fn register_mock_tempo_state_reader<DB: alloy_evm::Database>(
    evm: &mut TempoEvm<DB>,
) {
    use alloy_evm::precompiles::DynPrecompile;
    use alloy_sol_types::SolCall;
    use revm::precompile::{PrecompileId, PrecompileOutput};

    alloy_sol_types::sol! {
        function readStorageAt(address account, bytes32 slot, uint64 blockNumber) external view returns (bytes32);
        function readStorageBatchAt(address account, bytes32[] calldata slots, uint64 blockNumber) external view returns (bytes32[] memory);
    }

    let mock = DynPrecompile::new_stateful(
        PrecompileId::Custom("MockTempoStateReader".into()),
        move |input| {
            let data = input.data;
            if data.len() < 4 {
                return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
            }

            let selector: [u8; 4] = data[..4].try_into().expect("len >= 4");

            if selector == readStorageAtCall::SELECTOR {
                let encoded =
                    readStorageAtCall::abi_encode_returns(&alloy_primitives::B256::ZERO);
                Ok(PrecompileOutput::new(400, encoded.into()))
            } else if selector == readStorageBatchAtCall::SELECTOR {
                let call = readStorageBatchAtCall::abi_decode(data)
                    .map_err(|_| {
                        revm::precompile::PrecompileError::other("ABI decode failed")
                    })?;
                let zeros = vec![alloy_primitives::B256::ZERO; call.slots.len()];
                let encoded = readStorageBatchAtCall::abi_encode_returns(&zeros);
                Ok(PrecompileOutput::new(
                    200 + 200 * call.slots.len() as u64,
                    encoded.into(),
                ))
            } else {
                Ok(PrecompileOutput::new_reverted(0, Bytes::new()))
            }
        },
    );

    let (_, _, precompiles) = evm.components_mut();
    precompiles.apply_precompile(&TEMPO_STATE_READER_ADDRESS, |_| Some(mock));
}
