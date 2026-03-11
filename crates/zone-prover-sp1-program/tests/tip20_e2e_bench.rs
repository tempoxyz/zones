//! End-to-end single-block benchmark for zone proving with synthetic TIP-20 transfers.
//!
//! This builds a zone block entirely in-memory (no local node / network required), then:
//! - assembles a `BatchWitness`
//! - runs the native (soft) zone prover
//! - optionally runs an SP1 mock proof (offline) and verifies it
//! - optionally runs a Succinct network proof, verifies it, and prints request cost metrics
//!
//! Configure via environment variables:
//! - `ZONE_TIP20_BENCH_COUNTS=1,10,100,1000,10000` (or `MIN/MAX/STEP`)
//! - `ZONE_TIP20_BENCH_BACKEND=soft|sp1-mock|succinct|all`
//! - `ZONE_TIP20_BENCH_SKIP_SIMULATION=true|false` (succinct only)
//! - `ZONE_TIP20_BENCH_POLL_MS=5000` (succinct only)
//! - `ZONE_TIP20_BENCH_TIMEOUT_SECS=14400` (succinct only)
//!
//! Run:
//! `cargo test -p zone-prover-sp1-program --test tip20_e2e_bench -- --ignored --nocapture`

use std::{
    str::FromStr,
    time::{Duration, Instant},
};

use alloy_consensus::{Signed, TxLegacy, transaction::SignableTransaction};
use alloy_eips::eip2935;
use alloy_evm::{Evm, EvmEnv, EvmFactory, FromRecoveredTx};
use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256, map::HashMap, uint};
use alloy_rlp::{Decodable, Encodable};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, sol};
use revm::{
    DatabaseCommit,
    context::{BlockEnv, TxEnv},
    database::{CacheDB, EmptyDB},
    state::AccountInfo,
};
use sp1_sdk::{ProveRequest as _, Prover as _, ProverClient, ProvingKey as _, SP1Stdin};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_evm::evm::{TempoEvm, TempoEvmFactory};
use tempo_primitives::TempoTxEnvelope;
use tempo_revm::{TempoBlockEnv, TempoTxEnv};
use tokio::time::sleep;
use zone_prover::{
    execute, prove_zone_batch,
    testutil::{TestAccount, build_zone_state_fixture_with_absent, compute_state_root},
    types::{
        BatchStateProof, BatchWitness, PublicInputs, ZoneBlock, ZoneHeader,
        finalizeWithdrawalBatchCall,
    },
};
use zone_test_utils::{DEPLOYER, deploy_contract, extract_db_accounts, setup_zone_evm};

const CHAIN_ID: u64 = 13371;
const SEQUENCER: Address = Address::ZERO;
const TIP403_REGISTRY_ADDRESS: Address = address!("0x403c000000000000000000000000000000000000");
const TOKEN_ADDRESS: Address = address!("0x1111000000000000000000000000000000000001");
const TOKEN_ADMIN: Address = address!("0x9999000000000000000000000000000000000001");
const FEE_MANAGER_ADDRESS: Address = address!("0xfeEC000000000000000000000000000000000000");
const STABLECOIN_DEX_ADDRESS: Address = address!("0xDEc0000000000000000000000000000000000000");
const TEMPO_FEE_TOKEN_ADDRESS: Address = address!("0x20C0000000000000000000000000000000000000");
const FEE_MANAGER_BENCH_SLOT: U256 =
    uint!(0xa3c1274aadd82e4d12c8004c33fb244ca686dad4fcc8957fc5668588c11d9502_U256);
const TEMPO_FEE_TOKEN_BENCH_SLOT: U256 =
    uint!(0xcb8911fb82c2d10f6cf1d31d1e521ad3f4e3f42615f6ba67c454a9a2fdb9b6a7_U256);
const SYSTEM_SENDER: Address = Address::ZERO;
const USER_SK: alloy_primitives::B256 =
    alloy_primitives::b256!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

sol! {
    interface ITip20Setup {
        function grantRole(bytes32 role, address account) external;
        function mint(address to, uint256 amount) external;
        function transfer(address to, uint256 amount) external returns (bool);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendMode {
    Soft,
    Sp1Mock,
    Succinct,
    All,
}

impl BackendMode {
    fn wants_soft(self) -> bool {
        matches!(
            self,
            Self::Soft | Self::All | Self::Sp1Mock | Self::Succinct
        )
    }

    fn wants_sp1_mock(self) -> bool {
        matches!(self, Self::Sp1Mock | Self::All)
    }

    fn wants_succinct(self) -> bool {
        matches!(self, Self::Succinct | Self::All)
    }
}

impl FromStr for BackendMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "soft" => Ok(Self::Soft),
            "sp1-mock" | "mock" => Ok(Self::Sp1Mock),
            "succinct" | "network" => Ok(Self::Succinct),
            "all" | "both" => Ok(Self::All),
            _ => Err(format!("invalid backend mode: {s}")),
        }
    }
}

#[derive(Debug, Clone)]
struct BenchCase {
    transfer_count: usize,
    tx_bytes_total: usize,
    witness: BatchWitness,
}

#[derive(Debug, Clone)]
struct SoftRunMetrics {
    prove_ms: u128,
    output_bytes: Vec<u8>,
    healed_expected_state_root: bool,
}

#[derive(Debug, Clone)]
struct Sp1RunMetrics {
    prove_ms: u128,
    verify_ms: u128,
    proof_size_bytes: usize,
    public_values_len: usize,
}

#[derive(Debug, Clone)]
struct SuccinctRunMetrics {
    request_id: String,
    request_ms: u128,
    total_ms: u128,
    verify_ms: u128,
    proof_size_bytes: usize,
    public_values_len: usize,
    cycle_limit: u64,
    cycles: Option<u64>,
    gas_limit: u64,
    gas_used: Option<u64>,
    gas_price: Option<u64>,
    deduction_amount: Option<String>,
    refund_amount: Option<String>,
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_optional_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
}

fn default_succinct_gas_limit(transfer_count: usize) -> u64 {
    // Empirical fallback for skip-simulation mode:
    // - small transfer counts fit inside SP1's 1e9 default
    // - larger batches need substantially more headroom (observed ExceededGasLimit at 1000)
    match transfer_count {
        0..=100 => 1_000_000_000,
        101..=1000 => 10_000_000_000,
        _ => 50_000_000_000,
    }
}

fn parse_counts() -> Vec<usize> {
    if let Ok(csv) = std::env::var("ZONE_TIP20_BENCH_COUNTS") {
        let mut counts = Vec::new();
        for raw in csv.split(',') {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let n = trimmed
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid count in ZONE_TIP20_BENCH_COUNTS: {trimmed}"));
            assert!(
                (1..=10_000).contains(&n),
                "transfer count must be in [1,10000], got {n}"
            );
            counts.push(n);
        }
        assert!(
            !counts.is_empty(),
            "ZONE_TIP20_BENCH_COUNTS parsed to empty list"
        );
        return counts;
    }

    let min = env_u64("ZONE_TIP20_BENCH_MIN", 1) as usize;
    let max = env_u64("ZONE_TIP20_BENCH_MAX", min as u64) as usize;
    let step = env_u64("ZONE_TIP20_BENCH_STEP", 1) as usize;
    assert!(step > 0, "ZONE_TIP20_BENCH_STEP must be > 0");
    assert!(
        (1..=10_000).contains(&min) && (1..=10_000).contains(&max) && min <= max,
        "ZONE_TIP20_BENCH_MIN/MAX must satisfy 1 <= min <= max <= 10000"
    );

    (min..=max).step_by(step).collect()
}

fn parse_backend_mode() -> BackendMode {
    std::env::var("ZONE_TIP20_BENCH_BACKEND")
        .ok()
        .as_deref()
        .map(BackendMode::from_str)
        .transpose()
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or(BackendMode::Soft)
}

fn recipient_for(i: usize) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = 0x11;
    bytes[12..].copy_from_slice(&((i as u64) + 1).to_be_bytes());
    Address::from(bytes)
}

fn snapshot_to_test_account(snap: &zone_test_utils::AccountSnapshot) -> TestAccount {
    let code = snap.code.as_ref().and_then(|c| {
        let is_effectively_empty = c.is_empty() || (c.len() == 1 && c[0] == 0);
        if is_effectively_empty && snap.code_hash == alloy_primitives::KECCAK256_EMPTY {
            None
        } else {
            Some(c.clone())
        }
    });

    TestAccount {
        nonce: snap.nonce,
        balance: snap.balance,
        code_hash: snap.code_hash,
        code,
        storage: snap.storage.clone(),
    }
}

fn find_storage_value(
    accounts: &[(Address, TestAccount)],
    target_addr: Address,
    slot: U256,
) -> U256 {
    accounts
        .iter()
        .find(|(a, _)| *a == target_addr)
        .and_then(|(_, acct)| {
            acct.storage
                .iter()
                .find(|(s, _)| *s == slot)
                .map(|(_, v)| *v)
        })
        .unwrap_or(U256::ZERO)
}

fn build_witness_with_accessed_slots(
    pre_accounts: &[(Address, TestAccount)],
    accessed_slots: &[(Address, U256)],
    absent_addresses: &[Address],
) -> zone_prover::testutil::ZoneStateFixture {
    let mut enriched = pre_accounts.to_vec();
    for (addr, slot) in accessed_slots {
        if let Some((_, acct)) = enriched.iter_mut().find(|(a, _)| a == addr)
            && !acct.storage.iter().any(|(s, _)| s == slot)
        {
            acct.storage.push((*slot, U256::ZERO));
        }
    }
    build_zone_state_fixture_with_absent(&enriched, absent_addresses)
}

fn filter_real_accounts(
    post_accounts: Vec<(Address, TestAccount)>,
    pre_accounts: &[(Address, TestAccount)],
) -> Vec<(Address, TestAccount)> {
    post_accounts
        .into_iter()
        .filter(|(addr, acct)| {
            let in_pre = pre_accounts.iter().any(|(a, _)| a == addr);
            let is_non_empty = acct.nonce > 0
                || !acct.balance.is_zero()
                || acct.code_hash != alloy_primitives::KECCAK256_EMPTY;
            in_pre || is_non_empty
        })
        .collect()
}

fn collect_accessed_slots(post_accounts: &[(Address, TestAccount)]) -> Vec<(Address, U256)> {
    let mut slots: Vec<(Address, U256)> = post_accounts
        .iter()
        .flat_map(|(addr, acct)| acct.storage.iter().map(move |(slot, _)| (*addr, *slot)))
        .collect();

    let mandatory = [
        (
            execute::TEMPO_STATE_ADDRESS,
            execute::storage::TEMPO_STATE_BLOCK_HASH_SLOT,
        ),
        (
            execute::TEMPO_STATE_ADDRESS,
            execute::storage::TEMPO_STATE_STATE_ROOT_SLOT,
        ),
        (
            execute::TEMPO_STATE_ADDRESS,
            execute::storage::TEMPO_STATE_PACKED_SLOT,
        ),
        (
            execute::ZONE_INBOX_ADDRESS,
            execute::storage::ZONE_INBOX_PROCESSED_HASH_SLOT,
        ),
        (
            execute::ZONE_OUTBOX_ADDRESS,
            execute::storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT,
        ),
        (
            execute::ZONE_OUTBOX_ADDRESS,
            execute::storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1),
        ),
        (FEE_MANAGER_ADDRESS, FEE_MANAGER_BENCH_SLOT),
        (TEMPO_FEE_TOKEN_ADDRESS, TEMPO_FEE_TOKEN_BENCH_SLOT),
    ];

    for (addr, slot) in mandatory {
        if !slots.iter().any(|(a, s)| *a == addr && *s == slot) {
            slots.push((addr, slot));
        }
    }

    slots
}

fn next_deployer_nonce(evm: &mut TempoEvm<CacheDB<EmptyDB>>) -> u64 {
    evm.db_mut()
        .cache
        .accounts
        .get(&DEPLOYER)
        .map(|acct| acct.info.nonce)
        .unwrap_or(0)
}

fn ensure_account(evm: &mut TempoEvm<CacheDB<EmptyDB>>, addr: Address) {
    if !evm.db_mut().cache.accounts.contains_key(&addr) {
        evm.db_mut()
            .insert_account_info(addr, AccountInfo::default());
    }
}

fn transact_call(
    evm: &mut TempoEvm<CacheDB<EmptyDB>>,
    caller: Address,
    nonce: u64,
    to: Address,
    data: Vec<u8>,
) {
    ensure_account(evm, caller);

    let tx = TempoTxEnv {
        inner: TxEnv {
            caller,
            gas_price: 0,
            gas_limit: 3_000_000,
            kind: alloy_primitives::TxKind::Call(to),
            data: Bytes::from(data),
            chain_id: Some(CHAIN_ID),
            nonce,
            ..Default::default()
        },
        ..Default::default()
    };

    let result = evm.transact_raw(tx).expect("setup call tx should execute");
    assert!(
        result.result.is_success(),
        "setup call reverted/halted: {:?}",
        result.result
    );
    evm.db_mut().commit(result.state);
}

fn deploy_tip403_and_token(evm: &mut TempoEvm<CacheDB<EmptyDB>>, user: Address, mint_amount: U256) {
    ensure_account(evm, SYSTEM_SENDER);
    ensure_account(evm, TOKEN_ADMIN);
    ensure_account(evm, user);
    ensure_account(evm, FEE_MANAGER_ADDRESS);
    ensure_account(evm, STABLECOIN_DEX_ADDRESS);
    ensure_account(evm, TEMPO_FEE_TOKEN_ADDRESS);

    let mut nonce = next_deployer_nonce(evm);

    let registry_bytecode = zone_test_utils::load_artifact("TIP403Registry");
    deploy_contract(
        evm,
        &registry_bytecode,
        &[],
        TIP403_REGISTRY_ADDRESS,
        "TIP403Registry",
        CHAIN_ID,
        nonce,
    );
    nonce += 1;

    let token_bytecode = zone_test_utils::load_artifact("TIP20");
    let ctor_args = alloy_sol_types::SolValue::abi_encode_params(&(
        String::from("BenchToken"),
        String::from("BTK"),
        String::from("USD"),
        Address::ZERO, // no quote token needed for transfer benchmark
        TOKEN_ADMIN,
        TOKEN_ADMIN,
    ));
    deploy_contract(
        evm,
        &token_bytecode,
        &ctor_args,
        TOKEN_ADDRESS,
        "TIP20",
        CHAIN_ID,
        nonce,
    );

    let issuer_role = keccak256("ISSUER_ROLE");
    transact_call(
        evm,
        TOKEN_ADMIN,
        0,
        TOKEN_ADDRESS,
        ITip20Setup::grantRoleCall {
            role: issuer_role,
            account: TOKEN_ADMIN,
        }
        .abi_encode(),
    );

    transact_call(
        evm,
        TOKEN_ADMIN,
        1,
        TOKEN_ADDRESS,
        ITip20Setup::mintCall {
            to: user,
            amount: mint_amount,
        }
        .abi_encode(),
    );
}

fn build_transfer_txs(count: usize) -> (Address, Vec<Vec<u8>>) {
    let signer = PrivateKeySigner::from_bytes(&USER_SK).expect("valid test key");
    let user = signer.address();

    let mut txs = Vec::with_capacity(count);
    for i in 0..count {
        let to = recipient_for(i);
        let calldata = ITip20Setup::transferCall {
            to,
            amount: U256::from(1u64),
        }
        .abi_encode();

        let tx = TxLegacy {
            chain_id: Some(CHAIN_ID),
            nonce: i as u64,
            gas_price: 0,
            gas_limit: 200_000,
            to: alloy_primitives::TxKind::Call(TOKEN_ADDRESS),
            value: U256::ZERO,
            input: Bytes::from(calldata),
        };

        let sig_hash = tx.signature_hash();
        let sig = SignerSync::sign_hash_sync(&signer, &sig_hash).expect("sign");
        let signed = Signed::new_unchecked(tx, sig, sig_hash);
        let env = TempoTxEnvelope::Legacy(signed);

        let mut raw = Vec::new();
        env.encode(&mut raw);
        txs.push(raw);
    }

    (user, txs)
}

fn execute_on_cache_db(
    db: CacheDB<EmptyDB>,
    block: &ZoneBlock,
    is_last_block: bool,
) -> Vec<(Address, TestAccount)> {
    let block_env = TempoBlockEnv {
        inner: BlockEnv {
            number: U256::from(block.number),
            beneficiary: block.beneficiary,
            timestamp: U256::from(block.timestamp),
            gas_limit: block.gas_limit,
            basefee: block.base_fee_per_gas,
            ..Default::default()
        },
        timestamp_millis_part: 0,
    };

    let mut cfg_env = revm::context::CfgEnv::new_with_spec(TempoHardfork::T0);
    cfg_env.chain_id = CHAIN_ID;
    cfg_env.tx_gas_limit_cap = Some(u64::MAX);

    let env = EvmEnv { cfg_env, block_env };
    let factory = TempoEvmFactory::default();
    let mut evm = factory.create_evm(db, env);

    if block.number > 0 {
        let result = evm
            .transact_system_call(
                alloy_eips::eip4788::SYSTEM_ADDRESS,
                eip2935::HISTORY_STORAGE_ADDRESS,
                block.parent_hash.0.into(),
            )
            .expect("EIP-2935 system call should succeed");
        evm.db_mut().commit(result.state);
    }

    for raw_tx in &block.transactions {
        let envelope: TempoTxEnvelope =
            Decodable::decode(&mut raw_tx.as_slice()).expect("valid TempoTxEnvelope RLP");
        use reth_primitives_traits::SignerRecoverable;
        let recovered = envelope
            .try_into_recovered()
            .expect("signature recovery should succeed");
        let tx_env = <TempoTxEnv as FromRecoveredTx<TempoTxEnvelope>>::from_recovered_tx(
            recovered.inner(),
            recovered.signer(),
        );
        let result = evm.transact_raw(tx_env).expect("user tx should execute");
        assert!(
            result.result.is_success(),
            "user tx failed: {:?}",
            result.result
        );
        evm.db_mut().commit(result.state);
    }

    if is_last_block && let Some(count) = block.finalize_withdrawal_batch_count {
        let calldata = finalizeWithdrawalBatchCall {
            count,
            blockNumber: block.number,
        }
        .abi_encode();
        let result = evm
            .transact_system_call(Address::ZERO, execute::ZONE_OUTBOX_ADDRESS, calldata.into())
            .expect("finalizeWithdrawalBatch should succeed");
        assert!(
            result.result.is_success(),
            "finalize failed: {:?}",
            result.result
        );
        evm.db_mut().commit(result.state);
    }

    let (post_db, _) = evm.finish();
    post_db
        .cache
        .accounts
        .iter()
        .map(|(addr, acct)| {
            let info = &acct.info;
            let code = info.code.as_ref().map(|c| c.bytes().to_vec());
            let storage: Vec<(U256, U256)> = acct.storage.iter().map(|(k, v)| (*k, *v)).collect();
            (
                *addr,
                TestAccount {
                    nonce: info.nonce,
                    balance: info.balance,
                    code_hash: info.code_hash,
                    code,
                    storage,
                },
            )
        })
        .collect()
}

fn build_case(transfer_count: usize) -> BenchCase {
    let (user_address, txs) = build_transfer_txs(transfer_count);
    let total_mint = U256::from((transfer_count as u64) + 100);

    let mut evm = setup_zone_evm(CHAIN_ID);
    deploy_tip403_and_token(&mut evm, user_address, total_mint);

    let (db, _) = evm.finish();

    let pre_accounts: Vec<(Address, TestAccount)> = extract_db_accounts(&db)
        .into_iter()
        .map(|(addr, snap)| (addr, snapshot_to_test_account(&snap)))
        .collect();

    let placeholder_root = compute_state_root(&pre_accounts);
    let genesis_header = ZoneHeader {
        parent_hash: B256::ZERO,
        beneficiary: SEQUENCER,
        state_root: placeholder_root,
        transactions_root: B256::ZERO,
        receipts_root: B256::ZERO,
        number: 0,
        timestamp: 0,
    };
    let genesis_hash = genesis_header.block_hash();

    let zone_block = ZoneBlock {
        number: 1,
        parent_hash: genesis_hash,
        timestamp: 1_000,
        beneficiary: SEQUENCER,
        gas_limit: u64::MAX,
        base_fee_per_gas: 0,
        tempo_header_rlp: None,
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: Some(U256::ZERO),
        transactions: txs.clone(),
        expected_state_root: B256::ZERO,
    };

    let raw_post_accounts = execute_on_cache_db(db, &zone_block, true);
    let accessed_slots = collect_accessed_slots(&raw_post_accounts);
    let mut absent_addresses: Vec<Address> = raw_post_accounts
        .iter()
        .map(|(addr, _)| *addr)
        .filter(|addr| !pre_accounts.iter().any(|(a, _)| a == addr))
        .collect();
    if !absent_addresses.contains(&alloy_eips::eip4788::SYSTEM_ADDRESS) {
        absent_addresses.push(alloy_eips::eip4788::SYSTEM_ADDRESS);
    }

    let fixture =
        build_witness_with_accessed_slots(&pre_accounts, &accessed_slots, &absent_addresses);

    let genesis_header = ZoneHeader {
        state_root: fixture.state_root,
        ..genesis_header
    };
    let genesis_hash = genesis_header.block_hash();

    let post_accounts = filter_real_accounts(raw_post_accounts, &pre_accounts);
    let expected_state_root = compute_state_root(&post_accounts);

    let zone_block = ZoneBlock {
        parent_hash: genesis_hash,
        expected_state_root,
        ..zone_block
    };

    let tempo_packed = find_storage_value(
        &pre_accounts,
        execute::TEMPO_STATE_ADDRESS,
        execute::storage::TEMPO_STATE_PACKED_SLOT,
    );
    let tempo_block_number = execute::storage::extract_tempo_block_number(tempo_packed);
    let tempo_block_hash = B256::from(
        find_storage_value(
            &pre_accounts,
            execute::TEMPO_STATE_ADDRESS,
            execute::storage::TEMPO_STATE_BLOCK_HASH_SLOT,
        )
        .to_be_bytes(),
    );
    let outbox_wbi = find_storage_value(
        &pre_accounts,
        execute::ZONE_OUTBOX_ADDRESS,
        execute::storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1),
    )
    .to::<u64>();

    let public_inputs = PublicInputs {
        prev_block_hash: genesis_hash,
        tempo_block_number,
        anchor_block_number: tempo_block_number,
        anchor_block_hash: tempo_block_hash,
        expected_withdrawal_batch_index: outbox_wbi,
        sequencer: SEQUENCER,
    };

    let witness = BatchWitness {
        public_inputs,
        chain_id: CHAIN_ID,
        prev_block_header: genesis_header,
        zone_blocks: vec![zone_block],
        initial_zone_state: fixture.witness,
        tempo_state_proofs: BatchStateProof {
            node_pool: HashMap::default(),
            reads: vec![],
            account_proofs: vec![],
        },
        tempo_ancestry_headers: vec![],
    };

    BenchCase {
        transfer_count,
        tx_bytes_total: txs.iter().map(Vec::len).sum(),
        witness,
    }
}

fn encode_batch_output(output: &zone_prover::types::BatchOutput) -> Vec<u8> {
    let mut buf = Vec::with_capacity(192);
    buf.extend_from_slice(output.block_transition.prev_block_hash.as_slice());
    buf.extend_from_slice(output.block_transition.next_block_hash.as_slice());
    buf.extend_from_slice(
        output
            .deposit_queue_transition
            .prev_processed_hash
            .as_slice(),
    );
    buf.extend_from_slice(
        output
            .deposit_queue_transition
            .next_processed_hash
            .as_slice(),
    );
    buf.extend_from_slice(output.withdrawal_queue_hash.as_slice());
    buf.extend_from_slice(
        &U256::from(output.last_batch.withdrawal_batch_index).to_be_bytes::<32>(),
    );
    buf
}

fn parse_computed_state_root(msg: &str) -> Option<B256> {
    let marker = "computed=";
    let start = msg.find(marker)? + marker.len();
    let rest = &msg[start..];
    let end = rest.find(',').unwrap_or(rest.len());
    B256::from_str(rest[..end].trim()).ok()
}

fn run_soft(case: &mut BenchCase) -> SoftRunMetrics {
    let start = Instant::now();
    let mut healed_expected_state_root = false;
    let output = match prove_zone_batch(case.witness.clone()) {
        Ok(output) => output,
        Err(zone_prover::types::ProverError::InconsistentState(msg)) => {
            let computed = parse_computed_state_root(&msg).unwrap_or_else(|| {
                panic!("soft prove failed with unexpected InconsistentState format: {msg}")
            });
            let block = case
                .witness
                .zone_blocks
                .first_mut()
                .expect("single-block benchmark must contain one block");
            block.expected_state_root = computed;
            healed_expected_state_root = true;
            prove_zone_batch(case.witness.clone()).unwrap_or_else(|e| {
                panic!("soft prove should succeed after expected_state_root heal: {e}")
            })
        }
        Err(e) => panic!("soft prove_zone_batch should succeed: {e}"),
    };
    let prove_ms = start.elapsed().as_millis();
    let output_bytes = encode_batch_output(&output);
    assert_eq!(output_bytes.len(), 192, "soft output bytes must be 192");
    SoftRunMetrics {
        prove_ms,
        output_bytes,
        healed_expected_state_root,
    }
}

async fn run_sp1_mock(case: &BenchCase, expected_public_values: &[u8]) -> Sp1RunMetrics {
    let prover = ProverClient::builder().mock().build().await;
    let pk = prover
        .setup(zone_prover_sp1_program::ELF)
        .await
        .expect("SP1 mock setup should succeed");

    let mut stdin = SP1Stdin::new();
    stdin.write(&case.witness);

    let prove_start = Instant::now();
    let proof = prover
        .prove(&pk, stdin)
        .plonk()
        .await
        .expect("SP1 mock proof should succeed");
    let prove_ms = prove_start.elapsed().as_millis();

    let verify_start = Instant::now();
    prover
        .verify(&proof, pk.verifying_key(), None)
        .expect("SP1 mock verify should succeed");
    let verify_ms = verify_start.elapsed().as_millis();

    let public_values = proof.public_values.to_vec();
    assert_eq!(
        public_values, expected_public_values,
        "SP1 mock public values must match soft batch output encoding"
    );

    Sp1RunMetrics {
        prove_ms,
        verify_ms,
        proof_size_bytes: proof.bytes().len(),
        public_values_len: public_values.len(),
    }
}

async fn run_succinct(
    case: &BenchCase,
    expected_public_values: &[u8],
    skip_simulation: bool,
    poll_ms: u64,
    timeout_secs: u64,
) -> SuccinctRunMetrics {
    let gas_limit_override = env_optional_u64("ZONE_TIP20_BENCH_SUCCINCT_GAS_LIMIT");
    let cycle_limit_override = env_optional_u64("ZONE_TIP20_BENCH_SUCCINCT_CYCLE_LIMIT");
    let fallback_gas_limit = if skip_simulation && gas_limit_override.is_none() {
        Some(default_succinct_gas_limit(case.transfer_count))
    } else {
        None
    };

    println!(
        "succinct setup start: transfer_count={}",
        case.transfer_count
    );
    let prover = ProverClient::builder().network().build().await;
    println!(
        "succinct setup compiling keys: transfer_count={}",
        case.transfer_count
    );
    let pk = prover
        .setup(zone_prover_sp1_program::ELF)
        .await
        .expect("Succinct setup should succeed");
    let vk = pk.verifying_key().clone();
    println!(
        "succinct setup complete: transfer_count={}",
        case.transfer_count
    );

    let mut stdin = SP1Stdin::new();
    stdin.write(&case.witness);

    let total_start = Instant::now();
    let request_start = Instant::now();
    let mut request = prover
        .prove(&pk, stdin)
        .plonk()
        .skip_simulation(skip_simulation);
    if let Some(cycle_limit) = cycle_limit_override {
        request = request.cycle_limit(cycle_limit);
    }
    if let Some(gas_limit) = gas_limit_override.or(fallback_gas_limit) {
        request = request.gas_limit(gas_limit);
    }
    let request_id = request
        .request()
        .await
        .expect("Succinct proof request should succeed");
    let request_ms = request_start.elapsed().as_millis();

    let request_id_str = format!("{request_id}");
    println!(
        "succinct request submitted: transfer_count={} request_id={} skip_simulation={} gas_limit={:?} cycle_limit={:?}",
        case.transfer_count,
        request_id_str,
        skip_simulation,
        gas_limit_override.or(fallback_gas_limit),
        cycle_limit_override
    );
    let poll_interval = Duration::from_millis(poll_ms.max(250));
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(60));
    let mut latest_request = None;
    let mut last_status_log = Instant::now() - Duration::from_secs(60);
    let mut last_status = None;

    let proof = loop {
        let request = prover
            .get_proof_request(request_id)
            .await
            .expect("get_proof_request should succeed");
        if let Some(req) = request {
            latest_request = Some(req);
        }

        let (status, maybe_proof) = prover
            .get_proof_status(request_id)
            .await
            .expect("get_proof_status should succeed");
        let status_tuple = (status.fulfillment_status(), status.execution_status());
        if last_status != Some(status_tuple) || last_status_log.elapsed() >= Duration::from_secs(60)
        {
            println!(
                "succinct request status: transfer_count={} request_id={} fulfillment_status={} execution_status={}",
                case.transfer_count,
                request_id_str,
                status_tuple.0,
                status_tuple.1
            );
            last_status = Some(status_tuple);
            last_status_log = Instant::now();
        }

        if status.fulfillment_status() == 4 || status.execution_status() == 3 {
            let req = prover
                .get_proof_request(request_id)
                .await
                .expect("get_proof_request should succeed when request is terminal")
                .expect("proof request metadata should exist when request is terminal");
            panic!(
                "Succinct request {} failed (fulfillment_status={}, execution_status={}, execute_fail_cause={}, error={}, gas_limit={}, cycle_limit={})",
                request_id_str,
                status.fulfillment_status(),
                status.execution_status(),
                req.execute_fail_cause,
                req.error,
                req.gas_limit,
                req.cycle_limit
            );
        }

        if let Some(proof) = maybe_proof {
            let total_ms = total_start.elapsed().as_millis();
            let verify_start = Instant::now();
            prover
                .verify(&proof, &vk, None)
                .expect("Succinct proof verification should succeed");
            let verify_ms = verify_start.elapsed().as_millis();

            let public_values = proof.public_values.to_vec();
            assert_eq!(
                public_values, expected_public_values,
                "Succinct public values must match soft batch output encoding"
            );

            let req = if let Some(req) = latest_request {
                req
            } else {
                prover
                    .get_proof_request(request_id)
                    .await
                    .expect("final get_proof_request should succeed")
                    .expect("proof request metadata should be available once fulfilled")
            };

            break SuccinctRunMetrics {
                request_id: request_id_str,
                request_ms,
                total_ms,
                verify_ms,
                proof_size_bytes: proof.bytes().len(),
                public_values_len: public_values.len(),
                cycle_limit: req.cycle_limit,
                cycles: req.cycles,
                gas_limit: req.gas_limit,
                gas_used: req.gas_used,
                gas_price: req.gas_price,
                deduction_amount: req.deduction_amount,
                refund_amount: req.refund_amount,
            };
        }

        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for Succinct proof request {} after {}s (fulfillment_status={}, execution_status={})",
                request_id_str,
                timeout_secs,
                status.fulfillment_status(),
                status.execution_status()
            );
        }

        sleep(poll_interval).await;
    };

    proof
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark harness; optionally requires Succinct network credentials for backend=succinct/all"]
async fn bench_single_block_tip20_transfers() {
    let backend = parse_backend_mode();
    let counts = parse_counts();
    let skip_simulation = env_bool("ZONE_TIP20_BENCH_SKIP_SIMULATION", false);
    let poll_ms = env_u64("ZONE_TIP20_BENCH_POLL_MS", 5_000);
    let timeout_secs = env_u64("ZONE_TIP20_BENCH_TIMEOUT_SECS", 14_400);

    println!(
        "zone tip20 bench config: backend={backend:?} counts={counts:?} skip_simulation={skip_simulation} poll_ms={poll_ms} timeout_secs={timeout_secs}"
    );

    for &count in &counts {
        let build_start = Instant::now();
        let mut case = build_case(count);
        let build_ms = build_start.elapsed().as_millis();

        let soft_metrics = if backend.wants_soft() {
            Some(run_soft(&mut case))
        } else {
            None
        };

        let expected_public_values = soft_metrics
            .as_ref()
            .map(|m| m.output_bytes.clone())
            .unwrap_or_else(|| {
                let m = run_soft(&mut case);
                let bytes = m.output_bytes.clone();
                bytes
            });

        let sp1_mock_metrics = if backend.wants_sp1_mock() {
            Some(run_sp1_mock(&case, &expected_public_values).await)
        } else {
            None
        };

        let succinct_metrics = if backend.wants_succinct() {
            Some(
                run_succinct(
                    &case,
                    &expected_public_values,
                    skip_simulation,
                    poll_ms,
                    timeout_secs,
                )
                .await,
            )
        } else {
            None
        };

        let mut row = serde_json::Map::new();
        row.insert(
            "transfer_count".into(),
            serde_json::json!(case.transfer_count),
        );
        row.insert(
            "tx_bytes_total".into(),
            serde_json::json!(case.tx_bytes_total),
        );
        row.insert("witness_build_ms".into(), serde_json::json!(build_ms));

        if let Some(m) = soft_metrics {
            row.insert("soft_prove_ms".into(), serde_json::json!(m.prove_ms));
            row.insert(
                "soft_healed_expected_state_root".into(),
                serde_json::json!(m.healed_expected_state_root),
            );
            row.insert(
                "batch_output_public_values_len".into(),
                serde_json::json!(m.output_bytes.len()),
            );
        }

        if let Some(m) = sp1_mock_metrics {
            row.insert("sp1_mock_prove_ms".into(), serde_json::json!(m.prove_ms));
            row.insert("sp1_mock_verify_ms".into(), serde_json::json!(m.verify_ms));
            row.insert(
                "sp1_mock_proof_size_bytes".into(),
                serde_json::json!(m.proof_size_bytes),
            );
            row.insert(
                "sp1_mock_public_values_len".into(),
                serde_json::json!(m.public_values_len),
            );
        }

        if let Some(m) = succinct_metrics {
            row.insert(
                "succinct_request_id".into(),
                serde_json::json!(m.request_id),
            );
            row.insert(
                "succinct_request_ms".into(),
                serde_json::json!(m.request_ms),
            );
            row.insert("succinct_total_ms".into(), serde_json::json!(m.total_ms));
            row.insert("succinct_verify_ms".into(), serde_json::json!(m.verify_ms));
            row.insert(
                "succinct_proof_size_bytes".into(),
                serde_json::json!(m.proof_size_bytes),
            );
            row.insert(
                "succinct_public_values_len".into(),
                serde_json::json!(m.public_values_len),
            );
            row.insert(
                "succinct_cycle_limit".into(),
                serde_json::json!(m.cycle_limit),
            );
            row.insert("succinct_cycles".into(), serde_json::json!(m.cycles));
            row.insert("succinct_gas_limit".into(), serde_json::json!(m.gas_limit));
            row.insert("succinct_gas_used".into(), serde_json::json!(m.gas_used));
            row.insert("succinct_gas_price".into(), serde_json::json!(m.gas_price));
            row.insert(
                "succinct_deduction_amount".into(),
                serde_json::json!(m.deduction_amount),
            );
            row.insert(
                "succinct_refund_amount".into(),
                serde_json::json!(m.refund_amount),
            );
        }

        println!("{}", serde_json::Value::Object(row));
    }
}
