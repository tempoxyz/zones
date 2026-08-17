//! Zone genesis and portal storage-layout conformance tests.

use alloy_evm::{Evm, EvmEnv, EvmFactory};
use alloy_primitives::{Address, B256, U256, address, keccak256};
use revm::{
    context::result::{ExecutionResult, Output},
    database::{CacheDB, EmptyDB},
    state::AccountInfo,
};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_evm::evm::{TempoEvm, TempoEvmFactory};
use tempo_revm::TempoBlockEnv;
use zone_primitives::constants::{
    PORTAL_ADMIN_SLOT, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, PORTAL_ENCRYPTION_KEYS_SLOT,
    PORTAL_MAX_TEMPO_GAS_RATE_SLOT, PORTAL_ROLE_SLOT, PORTAL_TOKEN_ENABLEMENT_HASH_SLOT,
};

const TEMPO_STATE_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000000");
const ZONE_INBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000001");
const ZONE_OUTBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000002");

const DEPLOYER: Address = address!("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

/// Load a Foundry artifact's creation bytecode from the specs output directory.
fn load_artifact(name: &str) -> Vec<u8> {
    #[derive(serde::Deserialize)]
    struct FoundryArtifact {
        bytecode: BytecodeField,
    }
    #[derive(serde::Deserialize)]
    struct BytecodeField {
        object: String,
    }

    // Workspace root is two levels up from the crate directory
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let specs_out = manifest_dir.join("../../specs/ref-impls/out");
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
fn deploy_contract(
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

    println!("Deployed {name} at {predeploy_addr} (created at {created_addr})");
}

/// Build an EVM with the retained `ZoneInbox` Solidity artifact deployed in-memory.
fn setup_zone_inbox_for_portal(chain_id: u64, tempo_portal: Address) -> TempoEvm<CacheDB<EmptyDB>> {
    let gas_limit = 30_000_000u64;

    let db = CacheDB::default();
    let mut env: EvmEnv<TempoHardfork, TempoBlockEnv> =
        EvmEnv::default().with_timestamp(U256::ZERO);
    env.cfg_env.chain_id = chain_id;
    env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
    env.block_env.inner.gas_limit = gas_limit;

    let factory = TempoEvmFactory::default();
    let mut evm = factory.create_evm(db, env);

    // Fund the deployer
    evm.db_mut().insert_account_info(
        DEPLOYER,
        AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000_000u128),
            ..Default::default()
        },
    );

    // ZoneInbox(address tempoPortal, address tempoState)
    let zone_inbox_bytecode = load_artifact("ZoneInbox");
    let zone_inbox_args =
        alloy_sol_types::SolValue::abi_encode_params(&(tempo_portal, TEMPO_STATE_ADDRESS));
    deploy_contract(
        &mut evm,
        &zone_inbox_bytecode,
        &zone_inbox_args,
        ZONE_INBOX_ADDRESS,
        "ZoneInbox",
        chain_id,
        0,
    );

    evm
}

fn genesis_predeploy_code(addr: Address) -> Vec<u8> {
    let genesis: serde_json::Value =
        serde_json::from_str(zone_node::genesis::GENESIS_TEMPLATE_JSON)
            .expect("zone genesis template should parse");
    let key = format!("{addr:#x}");
    let code = genesis["alloc"][&key]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("missing code for {key} in zone-dev-genesis.json"));
    const_hex::decode(code.strip_prefix("0x").unwrap_or(code)).expect("decode genesis bytecode")
}

struct BytecodeView<'a> {
    raw: &'a [u8],
    compared: &'a [u8],
    metadata_footer_len: Option<usize>,
}

fn bytecode_view(bytecode: &[u8]) -> BytecodeView<'_> {
    let full_view = || BytecodeView {
        raw: bytecode,
        compared: bytecode,
        metadata_footer_len: None,
    };

    if bytecode.len() < 2 {
        return full_view();
    }

    let metadata_len =
        u16::from_be_bytes([bytecode[bytecode.len() - 2], bytecode[bytecode.len() - 1]]) as usize;
    let Some(metadata_start) = bytecode.len().checked_sub(metadata_len + 2) else {
        return full_view();
    };
    let metadata = &bytecode[metadata_start..bytecode.len() - 2];

    let looks_like_cbor_map = metadata.first().is_some_and(|byte| byte & 0xe0 == 0xa0);
    let has_solc_key = metadata
        .windows(b"solc".len())
        .any(|window| window == b"solc");
    if looks_like_cbor_map && has_solc_key {
        BytecodeView {
            raw: bytecode,
            compared: &bytecode[..metadata_start],
            metadata_footer_len: Some(metadata_len + 2),
        }
    } else {
        full_view()
    }
}

fn first_diff_index(left: &[u8], right: &[u8]) -> Option<usize> {
    left.iter()
        .zip(right)
        .position(|(left, right)| left != right)
        .or_else(|| (left.len() != right.len()).then_some(left.len().min(right.len())))
}

fn bytecode_hash(bytecode: &[u8]) -> String {
    format!("{:#x}", keccak256(bytecode))
}

fn metadata_footer_desc(metadata_footer_len: Option<usize>) -> String {
    metadata_footer_len.map_or_else(
        || "not detected".to_string(),
        |len| format!("stripped {len} bytes"),
    )
}

#[test]
fn zone_test_genesis_inbox_bytecode_matches_foundry_artifact() {
    // The L1 e2e tests boot the zone from this checked-in genesis fixture. If the
    // fixture falls behind the Solidity artifacts, ABI-breaking contract changes
    // can pass against old bytecode even though CI built the new artifacts. The
    // comparison ignores Solidity's CBOR metadata footer because metadata hashes
    // can differ across build environments while the executable bytecode matches.
    let mut evm = setup_zone_inbox_for_portal(1337, Address::ZERO);

    // TempoState and ZoneOutbox are native precompiles. The genesis generator intentionally uses
    // the non-empty native-account marker instead of deploying their Solidity reference shims.
    assert_eq!(genesis_predeploy_code(TEMPO_STATE_ADDRESS), [0xef]);
    assert_eq!(genesis_predeploy_code(ZONE_OUTBOX_ADDRESS), [0xef]);

    let (name, addr) = ("ZoneInbox", ZONE_INBOX_ADDRESS);
    let expected = evm
        .db_mut()
        .cache
        .accounts
        .get(&addr)
        .unwrap_or_else(|| panic!("{name} missing from generated EVM state"))
        .info
        .code
        .as_ref()
        .unwrap_or_else(|| panic!("{name} has no generated bytecode"))
        .original_bytes();
    let actual = genesis_predeploy_code(addr);
    let actual_view = bytecode_view(&actual);
    let expected_view = bytecode_view(expected.as_ref());

    if actual_view.compared != expected_view.compared {
        let first_diff = first_diff_index(actual_view.compared, expected_view.compared)
            .map_or_else(|| "none".to_string(), |index| index.to_string());
        panic!(
            "{name} bytecode in zone-dev-genesis.json does not match the freshly deployed \
             Foundry artifact at {addr:#x}\n\
             first compared-byte difference: {first_diff}\n\
             genesis: raw_len={} compared_len={} metadata_footer={} raw_keccak={} \
             compared_keccak={}\n\
             Foundry artifact: raw_len={} compared_len={} metadata_footer={} raw_keccak={} \
             compared_keccak={}\n\
             Compared bytecode excludes Solidity's CBOR metadata footer when present, because \
             that metadata hash can vary by build environment. A mismatch here means the \
             executable contract bytecode changed.\n\
             Refresh with: `forge build --root specs/ref-impls` then \
             `cargo run --manifest-path xtask/Cargo.toml -- generate-zone-genesis ...`",
            actual_view.raw.len(),
            actual_view.compared.len(),
            metadata_footer_desc(actual_view.metadata_footer_len),
            bytecode_hash(actual_view.raw),
            bytecode_hash(actual_view.compared),
            expected_view.raw.len(),
            expected_view.compared.len(),
            metadata_footer_desc(expected_view.metadata_footer_len),
            bytecode_hash(expected_view.raw),
            bytecode_hash(expected_view.compared)
        );
    }
}

/// Pins the Rust portal storage-slot constants to the ZonePortal storage layout.
#[test]
fn zone_portal_storage_slot_constants_match_solidity() {
    assert_eq!(PORTAL_ADMIN_SLOT, B256::ZERO, "admin is slot 0");
    assert_eq!(
        PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
        B256::from(U256::from(3)),
        "currentDepositQueueHash is slot 3"
    );
    assert_eq!(
        PORTAL_ENCRYPTION_KEYS_SLOT,
        B256::from(U256::from(5)),
        "_encryptionKeys is slot 5"
    );
    assert_eq!(
        PORTAL_ROLE_SLOT,
        B256::from(U256::from(20)),
        "role is slot 20"
    );
    assert_eq!(
        PORTAL_MAX_TEMPO_GAS_RATE_SLOT,
        B256::from(U256::from(22)),
        "maxTempoGasRate is slot 22"
    );
    assert_eq!(
        PORTAL_TOKEN_ENABLEMENT_HASH_SLOT,
        B256::from(U256::from(26)),
        "tokenEnablementHash is slot 26"
    );
}
