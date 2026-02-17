//! Reproduce the `advanceTempo` system tx reverting at 232 gas.
//!
//! Run with: `cargo test -p zone --test advance_tempo -- --nocapture`

use alloy_evm::{Evm, EvmEnv, EvmFactory};
use alloy_primitives::{Address, Bytes, U256, address};
use alloy_sol_types::{SolCall, sol};
use revm::{
    context::result::{ExecutionResult, Output},
    database::{CacheDB, EmptyDB},
    state::AccountInfo,
};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_evm::evm::{TempoEvm, TempoEvmFactory};
use tempo_revm::TempoBlockEnv;

const TEMPO_STATE_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000000");
const ZONE_INBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000001");
const ZONE_OUTBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000002");
const ZONE_CONFIG_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000003");

const DEPLOYER: Address = address!("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

sol! {
    function advanceTempo(bytes calldata header, QueuedDeposit[] calldata deposits, DecryptionData[] calldata decryptions);
    function config() external view returns (address);
    function tempoBlockHash() external view returns (bytes32);

    struct QueuedDeposit {
        uint8 depositType;
        bytes depositData;
    }
    struct ChaumPedersenProof {
        bytes32 s;
        bytes32 c;
    }
    struct DecryptionData {
        bytes32 sharedSecret;
        uint8 sharedSecretYParity;
        address to;
        bytes32 memo;
        ChaumPedersenProof cpProof;
    }
}

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
    let specs_out = manifest_dir.join("../../docs/specs/out");
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

/// Build an EVM with the zone contracts deployed in-memory (same as xtask generate_zone_genesis).
fn setup_zone_evm_with_contracts() -> TempoEvm<CacheDB<EmptyDB>> {
    let chain_id = 13371u64;
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

    // A minimal RLP-encoded Tempo genesis header (all zeros / minimal valid).
    // This is the same format used by TempoState constructor.
    // We create a dummy header RLP: an RLP list of empty/zero fields.
    // For testing, use a simple valid RLP that TempoState will accept.
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
    let zone_token = Address::repeat_byte(0xaa); // dummy
    let tempo_portal = Address::repeat_byte(0xbb); // dummy
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

    println!("All zone contracts deployed successfully");
    evm
}

/// Build a minimal valid RLP-encoded TempoHeader.
///
/// We look at the TempoHeader struct to determine which fields to encode.
/// For a minimal genesis header, most fields are zeroed.
fn build_dummy_header_rlp() -> Vec<u8> {
    use alloy_rlp::Encodable;
    use tempo_primitives::TempoHeader;

    // Construct a minimal TempoHeader (genesis-like)
    let header = TempoHeader::default();

    let mut buf = Vec::new();
    header.encode(&mut buf);
    buf
}

#[test]
fn advance_tempo_repro() {
    let mut evm = setup_zone_evm_with_contracts();

    // System txs use Address::ZERO which bypasses OnlySequencer in ZoneInbox/ZoneOutbox
    let sequencer = Address::ZERO;

    // ---------------------------------------------------------------
    // Step 1: Call config() on ZoneInbox — simple view call to verify contracts work
    // ---------------------------------------------------------------
    println!("\n=== Calling ZoneInbox.config() ===");
    let config_calldata = configCall {}.abi_encode();
    let config_result =
        evm.transact_system_call(sequencer, ZONE_INBOX_ADDRESS, Bytes::from(config_calldata));
    match &config_result {
        Ok(result) => {
            println!("config() result: {:?}", result.result);
            match &result.result {
                ExecutionResult::Success {
                    output, gas_used, ..
                } => {
                    if let Output::Call(data) = output {
                        println!("config() returned: {}", data);
                    }
                    println!("config() gas_used: {gas_used}");
                }
                ExecutionResult::Revert {
                    output, gas_used, ..
                } => {
                    println!("config() REVERTED: {output}, gas_used: {gas_used}");
                }
                ExecutionResult::Halt {
                    reason, gas_used, ..
                } => {
                    println!("config() HALTED: {reason:?}, gas_used: {gas_used}");
                }
            }
        }
        Err(e) => println!("config() ERROR: {e:?}"),
    }

    // ---------------------------------------------------------------
    // Step 2: Call tempoBlockHash() on TempoState to verify it works
    // ---------------------------------------------------------------
    println!("\n=== Calling TempoState.tempoBlockHash() ===");
    let hash_calldata = tempoBlockHashCall {}.abi_encode();
    let hash_result =
        evm.transact_system_call(sequencer, TEMPO_STATE_ADDRESS, Bytes::from(hash_calldata));
    match &hash_result {
        Ok(result) => match &result.result {
            ExecutionResult::Success {
                output, gas_used, ..
            } => {
                if let Output::Call(data) = output {
                    println!("tempoBlockHash() returned: {}", data);
                }
                println!("tempoBlockHash() gas_used: {gas_used}");
            }
            ExecutionResult::Revert {
                output, gas_used, ..
            } => {
                println!("tempoBlockHash() REVERTED: {output}, gas_used: {gas_used}");
            }
            other => println!("tempoBlockHash() other: {other:?}"),
        },
        Err(e) => println!("tempoBlockHash() ERROR: {e:?}"),
    }

    // ---------------------------------------------------------------
    // Step 3: Call advanceTempo with a minimal next header
    // ---------------------------------------------------------------
    // Verify contracts have code
    {
        let inbox_code = evm
            .db_mut()
            .cache
            .accounts
            .get(&ZONE_INBOX_ADDRESS)
            .and_then(|a| a.info.code.as_ref())
            .map(|c| c.len())
            .unwrap_or(0);
        let tempostate_code = evm
            .db_mut()
            .cache
            .accounts
            .get(&TEMPO_STATE_ADDRESS)
            .and_then(|a| a.info.code.as_ref())
            .map(|c| c.len())
            .unwrap_or(0);
        println!("ZoneInbox code size: {inbox_code}");
        println!("TempoState code size: {tempostate_code}");
    }

    // ---------------------------------------------------------------
    // Step 2.5: Call finalizeTempo directly on TempoState to isolate
    // ---------------------------------------------------------------
    println!("\n=== Building child header ===");

    // Build a "next" header that's a child of the genesis.
    // finalizeTempo requires: tempoParentHash == prev tempoBlockHash, tempoBlockNumber == prev + 1
    let genesis_hash = {
        use alloy_rlp::Encodable;
        let genesis = tempo_primitives::TempoHeader::default();
        let mut buf = Vec::new();
        genesis.encode(&mut buf);
        alloy_primitives::keccak256(&buf)
    };
    println!("Genesis hash (computed): {genesis_hash}");

    let next_header = tempo_primitives::TempoHeader {
        inner: alloy_consensus::Header {
            number: 1,
            parent_hash: genesis_hash,
            ..Default::default()
        },
        ..Default::default()
    };
    let next_header_rlp = {
        use alloy_rlp::Encodable;
        let mut buf = Vec::new();
        next_header.encode(&mut buf);
        buf
    };
    println!("Next header RLP length: {}", next_header_rlp.len());
    println!("Next header block number: {}", next_header.inner.number);
    println!("Next header parent hash: {}", next_header.inner.parent_hash);

    // NOTE: finalizeTempo() tested separately and works (86729 gas).
    // Skip calling it directly here to avoid corrupting state for the advanceTempo call.

    println!("\n=== Calling ZoneInbox.advanceTempo() ===");

    let advance_calldata = advanceTempoCall {
        header: Bytes::from(next_header_rlp),
        deposits: vec![],
        decryptions: vec![],
    }
    .abi_encode();

    println!(
        "advanceTempo calldata length: {} bytes",
        advance_calldata.len()
    );
    println!(
        "advanceTempo selector: 0x{}",
        const_hex::encode(&advance_calldata[..4])
    );

    let advance_result = evm.transact_system_call(
        sequencer,
        ZONE_INBOX_ADDRESS,
        Bytes::from(advance_calldata.clone()),
    );
    match &advance_result {
        Ok(result) => match &result.result {
            ExecutionResult::Success {
                output, gas_used, ..
            } => {
                if let Output::Call(data) = output {
                    println!("advanceTempo() SUCCESS, output: {}", data);
                }
                println!("advanceTempo() gas_used: {gas_used}");
            }
            ExecutionResult::Revert {
                output, gas_used, ..
            } => {
                println!("advanceTempo() REVERTED: {output}");
                println!("advanceTempo() gas_used: {gas_used}");
                if output.len() >= 4 {
                    let sel = &output[..4];
                    println!("  error selector: 0x{}", const_hex::encode(sel));
                    if sel == [0x08, 0xc3, 0x79, 0xa0] && output.len() > 4 {
                        if let Ok(msg) = <alloy_sol_types::sol_data::String as alloy_sol_types::SolType>::abi_decode(&output[4..]) {
                            println!("  Error message: {msg}");
                        }
                    }
                }
            }
            ExecutionResult::Halt {
                reason, gas_used, ..
            } => {
                println!("advanceTempo() HALTED: {reason:?}");
                println!("advanceTempo() gas_used: {gas_used}");
            }
        },
        Err(e) => println!("advanceTempo() ERROR: {e:?}"),
    }

    // The test should not panic; we want to see the output
    println!("\n=== Test complete ===");
}

