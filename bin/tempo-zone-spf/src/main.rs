use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{BlockHeader as _, Sealable as _, Transaction as _};
use alloy_eips::{BlockId, eip2718::Encodable2718 as _};
use alloy_network::primitives::BlockTransactions;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_types_eth::{Block, BlockNumberOrTag, Transaction};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall as _;
use clap::{Parser, Subcommand};
use eyre::{Context, Result, bail, eyre};
use futures::{StreamExt, TryStreamExt, stream};
use serde::Deserialize;
use serde_json::Value;
use tempo_alloy::{TempoNetwork, rpc::TempoHeaderResponse};
use tempo_chainspec::spec::chainspec_from_chain_id;
use tempo_primitives::{TempoHeader, TempoTxEnvelope};
use tempo_zone_contracts::{
    TEMPO_STATE_ADDRESS, TempoState, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZoneInbox,
    ZoneOutbox, ZonePortal,
};
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;
use zone_chainspec::ZoneChainSpec;
use zone_precompiles::tempo_state::slots as tempo_state_slots;
use zone_primitives::constants::zone_chain_id;
use zone_rpc::{ZoneProvider, ZoneProviderConfig};
use zone_spf::{
    BatchOutput, BatchWitness, PublicInputs, SpfConfig, TempoStateWitness, ZoneBlock,
    ZoneStateWitness, prove_zone_batch,
};

const EIP2935_HISTORY_WINDOW: u64 = 8191;
const EIP2935_SAFETY_MARGIN: u64 = 360;
const RPC_CONCURRENCY: usize = 8;
const PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT: B256 = B256::with_last_byte(5);

type RpcBlock = Block<Transaction<TempoTxEnvelope>, TempoHeaderResponse>;
type L1Reads = BTreeMap<u64, BTreeMap<Address, BTreeSet<B256>>>;

#[derive(Debug, Parser)]
#[command(name = "tempo-zone-spf", about = "Tempo Zone SPF development tools")]
struct Cli {
    /// Tracing filter. Can also be set with RUST_LOG.
    #[arg(
        long,
        global = true,
        env = "RUST_LOG",
        default_value = "tempo_zone_spf=info"
    )]
    log_filter: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate and locally validate an SPF batch witness.
    GenerateInput(GenerateInputArgs),
}

#[derive(Debug, clap::Args)]
struct GenerateInputArgs {
    /// Tempo L1 HTTP or WebSocket RPC URL.
    #[arg(long)]
    tempo_rpc_url: String,

    /// Authenticated private Zone HTTP RPC URL used for Zone discovery.
    #[arg(long)]
    zone_rpc_url: String,

    /// Unrestricted Zone RPC URL used for full blocks, state, and debug methods.
    #[arg(long)]
    zone_unrestricted_rpc_url: String,

    /// Private key used to authenticate with the private Zone RPC.
    #[arg(long, env = "PRIVATE_KEY", value_name = "HEX", hide_env_values = true)]
    private_key: String,

    /// Numeric Zone identifier used in the private RPC authorization token.
    #[arg(long)]
    zone_id: u32,

    /// Zone chain ID. Defaults to the chain ID canonically derived from `--zone-id`.
    #[arg(long)]
    zone_chain_id: Option<u64>,

    /// Override the first Zone block in the batch.
    #[arg(long)]
    from_block: Option<u64>,

    /// Override the final Zone block. Defaults to the current Zone tip.
    #[arg(long)]
    to_block: Option<u64>,

    /// Write the complete JSON witness to this path.
    #[arg(long, short)]
    output: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct Timings(Vec<(&'static str, Duration)>);

impl Timings {
    fn record<T>(&mut self, name: &'static str, started: Instant, value: T) -> T {
        let elapsed = started.elapsed();
        info!(
            phase = name,
            elapsed_ms = elapsed.as_millis(),
            "phase complete"
        );
        self.0.push((name, elapsed));
        value
    }

    fn print(&self, total: Duration) {
        println!("\nTiming");
        for (name, elapsed) in &self.0 {
            println!("  {name:<22} {}", format_duration(*elapsed));
        }
        println!("  {:<22} {}", "total", format_duration(total));
    }
}

#[derive(Debug)]
struct Discovery {
    zone_id: u32,
    portal: Address,
    sequencer: Address,
    portal_withdrawal_batch_index: u64,
    portal_tempo_block_number: u64,
    tempo_chain_id: u64,
    portal_block_hash: B256,
}

#[derive(Debug)]
struct ExtractedBlock {
    input: ZoneBlock,
    checkpoint_number: Option<u64>,
    has_finalization: bool,
    user_transaction_count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecutionWitness {
    #[serde(default)]
    state: Vec<Bytes>,
    #[serde(default)]
    codes: Vec<Bytes>,
    #[serde(default)]
    headers: Vec<Bytes>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log_filter)?;
    match cli.command {
        Command::GenerateInput(args) => generate_input(args).await,
    }
}

fn init_tracing(filter: &str) -> Result<()> {
    let filter = EnvFilter::try_new(filter).context("parse tracing filter")?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| eyre!("initialize tracing: {error}"))
}

fn start_phase(name: &'static str) -> Instant {
    info!(phase = name, "starting phase");
    Instant::now()
}

async fn generate_input(args: GenerateInputArgs) -> Result<()> {
    let total_started = Instant::now();
    let mut timings = Timings::default();
    info!(
        zone_id = args.zone_id,
        from_block = ?args.from_block,
        to_block = ?args.to_block,
        writes_output = args.output.is_some(),
        "generating SPF input"
    );

    let started = start_phase("discovery");
    let tempo_provider = connect(&args.tempo_rpc_url, "Tempo").await?;
    let zone_provider = connect(&args.zone_unrestricted_rpc_url, "unrestricted Zone").await?;
    let signer = args
        .private_key
        .parse::<PrivateKeySigner>()
        .context("parse private Zone RPC key")?;
    let expected_zone_chain_id = args
        .zone_chain_id
        .unwrap_or_else(|| zone_chain_id(args.zone_id));
    let private_zone_provider = connect_private_zone(
        &args.zone_rpc_url,
        signer,
        args.zone_id,
        expected_zone_chain_id,
    )?;
    let discovery = discover(
        &tempo_provider,
        &private_zone_provider,
        args.zone_id,
        expected_zone_chain_id,
    )
    .await?;
    validate_unrestricted_zone(&zone_provider, &discovery, expected_zone_chain_id).await?;
    info!(
        zone_id = discovery.zone_id,
        portal = %discovery.portal,
        sequencer = %discovery.sequencer,
        committed_zone_hash = %discovery.portal_block_hash,
        portal_tempo_block = discovery.portal_tempo_block_number,
        withdrawal_batch_index = discovery.portal_withdrawal_batch_index,
        "discovered Zone"
    );
    timings.record("discovery", started, ());

    let started = start_phase("batch extraction");
    let (parent_header, parent_number, extracted) =
        discover_batch(&zone_provider, &discovery, args.from_block, args.to_block).await?;
    let from_block = extracted
        .first()
        .expect("batch discovery returns a non-empty batch")
        .input
        .number;
    let to_block = extracted.last().expect("non-empty").input.number;
    let next_block_hash = zone_header(&zone_provider, to_block).await?.hash_slow();
    let finalization_count = extracted
        .iter()
        .filter(|block| block.has_finalization)
        .count();
    let expected_withdrawal_batch_index = discovery
        .portal_withdrawal_batch_index
        .checked_add(u64::try_from(finalization_count).expect("block count fits u64"))
        .ok_or_else(|| eyre!("withdrawal batch index overflow"))?;
    let (zone_head, tempo_head) = tokio::try_join!(
        zone_provider.get_block_number(),
        tempo_provider.get_block_number()
    )?;
    info!(
        from_block,
        to_block,
        zone_head,
        tempo_head,
        portal_tempo_block = discovery.portal_tempo_block_number,
        block_count = extracted.len(),
        finalization_count,
        "selected Zone block range"
    );
    timings.record("batch extraction", started, ());

    let started = start_phase("initial checkpoint");
    let initial_tempo_header =
        initial_tempo_header(&tempo_provider, &zone_provider, parent_number).await?;
    timings.record("initial checkpoint", started, ());

    let started = start_phase("Zone state witness");
    let (zone_state_witness, traces) = zone_witnesses(&zone_provider, from_block, to_block).await?;
    timings.record("Zone state witness", started, ());

    let started = start_phase("Tempo state witness");
    let portal = discovery.portal;
    let mut checkpoint_by_zone_block = BTreeMap::new();
    let mut checkpoint = initial_tempo_header.number();
    for block in &extracted {
        if let Some(imported) = block.checkpoint_number {
            checkpoint = imported;
        }
        checkpoint_by_zone_block.insert(block.input.number, checkpoint);
    }
    let mut reads = collect_l1_reads(&traces, &checkpoint_by_zone_block)?;
    for block in &extracted {
        let checkpoint = checkpoint_by_zone_block[&block.input.number];
        // advanceTempo always authenticates the portal deposit-queue head.
        reads
            .entry(checkpoint)
            .or_default()
            .entry(portal)
            .or_default()
            .insert(PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT);
    }
    let tempo_state_witness =
        tempo_state_witness(&tempo_provider, &initial_tempo_header, reads).await?;
    timings.record("Tempo state witness", started, ());

    let final_tempo_header = extracted
        .iter()
        .rev()
        .find_map(|block| block.input.tempo_header_rlp.as_ref())
        .map(|encoded| decode_tempo_header(encoded))
        .transpose()?
        .unwrap_or_else(|| initial_tempo_header.clone());

    let started = start_phase("Tempo ancestry");
    let (anchor_block_number, anchor_block_hash, tempo_ancestry_headers, anchor_mode) =
        tempo_anchor(&tempo_provider, &final_tempo_header).await?;
    timings.record("Tempo ancestry", started, ());

    let witness = BatchWitness {
        public_inputs: PublicInputs {
            zone_id: discovery.zone_id,
            tempo_block_number: final_tempo_header.number(),
            anchor_block_number,
            anchor_block_hash,
            expected_withdrawal_batch_index,
            sequencer: discovery.sequencer,
        },
        parent_header,
        zone_blocks: extracted.iter().map(|block| block.input.clone()).collect(),
        zone_state_witness,
        tempo_state_witness,
        tempo_ancestry_headers,
    };

    let started = start_phase("SPF validation");
    let output = validate_witness(discovery.tempo_chain_id, &witness)?;
    if output.block_transition.nextBlockHash != next_block_hash {
        bail!(
            "SPF replay produced {}, but Zone block {to_block} has hash {next_block_hash}",
            output.block_transition.nextBlockHash
        );
    }
    timings.record("SPF validation", started, ());

    let started = start_phase("output");
    let output_bytes = if let Some(path) = &args.output {
        let json = serde_json::to_vec_pretty(&witness).context("serialize batch witness")?;
        std::fs::write(path, &json)
            .wrap_err_with(|| format!("write SPF input to {}", path.display()))?;
        Some(json.len())
    } else {
        None
    };
    timings.record("output", started, ());

    print_summary(
        &discovery,
        &witness,
        &extracted,
        &output,
        anchor_mode,
        args.output.as_ref(),
        output_bytes,
    );
    timings.print(total_started.elapsed());
    Ok(())
}

async fn connect(url: &str, label: &str) -> Result<DynProvider<TempoNetwork>> {
    ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect(url)
        .await
        .wrap_err_with(|| format!("connect to {label} RPC at {url}"))
        .map(Provider::erased)
}

fn connect_private_zone(
    url: &str,
    signer: PrivateKeySigner,
    zone_id: u32,
    chain_id: u64,
) -> Result<DynProvider<TempoNetwork>> {
    let rpc_url = url
        .parse()
        .wrap_err_with(|| format!("parse private Zone RPC URL {url}"))?;
    ZoneProvider::new(ZoneProviderConfig {
        signer,
        zone_id,
        chain_id,
        token_ttl: Duration::from_secs(600),
        rpc_url,
    })
    .wrap_err_with(|| format!("connect to private Zone RPC at {url}"))
    .map(|provider| provider.provider())
}

async fn discover(
    tempo: &DynProvider<TempoNetwork>,
    private_zone: &DynProvider<TempoNetwork>,
    expected_zone_id: u32,
    expected_zone_chain_id: u64,
) -> Result<Discovery> {
    let (tempo_chain_id, zone_chain_id) =
        tokio::try_join!(tempo.get_chain_id(), private_zone.get_chain_id())?;
    if zone_chain_id != expected_zone_chain_id {
        bail!(
            "private Zone RPC reports chain ID {zone_chain_id}, but the auth token uses {expected_zone_chain_id}"
        );
    }
    let zone_id = private_zone
        .client()
        .request_noparams::<Value>("zone_getZoneInfo")
        .await
        .context("call authenticated zone_getZoneInfo")?
        .get("zoneId")
        .and_then(Value::as_str)
        .map(parse_quantity)
        .transpose()?
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| eyre!("zone_getZoneInfo returned no valid zoneId"))?;
    if zone_id != expected_zone_id {
        bail!("private Zone RPC reports Zone ID {zone_id}, but --zone-id is {expected_zone_id}");
    }

    let portal_address = ZoneInbox::new(ZONE_INBOX_ADDRESS, private_zone.clone())
        .tempoPortal()
        .call()
        .await
        .context("read Tempo portal address from ZoneInbox")?;
    if portal_address == Address::ZERO {
        bail!("ZoneInbox reports a zero Tempo portal address");
    }
    let portal = ZonePortal::new(portal_address, tempo.clone());
    let portal_zone_id_call = portal.zoneId();
    let sequencer_call = portal.sequencer();
    let withdrawal_batch_index_call = portal.withdrawalBatchIndex();
    let block_hash_call = portal.blockHash();
    let tempo_block_number_call = portal.lastSyncedTempoBlockNumber();
    let (
        portal_zone_id,
        sequencer,
        withdrawal_batch_index,
        portal_block_hash,
        portal_tempo_block_number,
    ) = tokio::try_join!(
        portal_zone_id_call.call(),
        sequencer_call.call(),
        withdrawal_batch_index_call.call(),
        block_hash_call.call(),
        tempo_block_number_call.call(),
    )?;
    if portal_zone_id != zone_id {
        bail!(
            "ZoneInbox points to Tempo portal {portal_address}, but that portal reports Zone ID {portal_zone_id} instead of {zone_id}"
        );
    }

    Ok(Discovery {
        zone_id,
        portal: portal_address,
        sequencer,
        portal_withdrawal_batch_index: withdrawal_batch_index,
        portal_tempo_block_number,
        tempo_chain_id,
        portal_block_hash,
    })
}

async fn validate_unrestricted_zone(
    zone: &DynProvider<TempoNetwork>,
    discovery: &Discovery,
    expected_chain_id: u64,
) -> Result<()> {
    let inbox = ZoneInbox::new(ZONE_INBOX_ADDRESS, zone.clone());
    let portal_call = inbox.tempoPortal();
    let (chain_id, portal) = tokio::join!(zone.get_chain_id(), portal_call.call());
    let chain_id = chain_id.context("read unrestricted Zone chain ID")?;
    let portal = portal.context("read Tempo portal from unrestricted Zone RPC")?;
    if chain_id != expected_chain_id {
        bail!(
            "unrestricted Zone RPC reports chain ID {chain_id}, but the private Zone RPC reports {expected_chain_id}"
        );
    }
    if portal != discovery.portal {
        bail!(
            "unrestricted Zone RPC points to Tempo portal {portal}, but the private Zone RPC points to {}",
            discovery.portal
        );
    }
    Ok(())
}

async fn discover_batch(
    zone: &DynProvider<TempoNetwork>,
    discovery: &Discovery,
    from_override: Option<u64>,
    to_override: Option<u64>,
) -> Result<(TempoHeader, u64, Vec<ExtractedBlock>)> {
    let portal_parent = match zone.get_block_by_hash(discovery.portal_block_hash).await? {
        Some(block) => block,
        None => {
            let tip = zone.get_block_number().await?;
            let genesis_hash = zone_header(zone, 0).await?.hash_slow();
            bail!(
                "Tempo portal {} commits Zone block hash {}, but the unrestricted Zone RPC does not contain it (Zone tip: {tip}, genesis hash: {genesis_hash}); the endpoint may belong to a reset or different Zone chain",
                discovery.portal,
                discovery.portal_block_hash
            );
        }
    };
    let portal_parent_number = portal_parent.header.number();
    let default_from = portal_parent_number
        .checked_add(1)
        .ok_or_else(|| eyre!("Zone block number overflow"))?;
    let from = from_override.unwrap_or(default_from);
    if from == 0 {
        bail!("a batch cannot start at Zone genesis block 0");
    }
    let parent_number = from - 1;
    let parent_header = zone_header(zone, parent_number).await?;
    if from_override.is_none() && parent_header.hash_slow() != discovery.portal_block_hash {
        bail!("resolved parent header does not match the portal block hash");
    }

    let tip = zone.get_block_number().await?;
    let limit = to_override.unwrap_or(tip);
    if from > limit {
        bail!("no Zone blocks available in requested range {from}..={limit}");
    }

    let mut blocks = Vec::new();
    for number in from..=limit {
        let block = fetch_full_block(zone, number).await?;
        let extracted = extract_block(block)?;
        debug!(
            zone_block = number,
            parent_hash = %extracted.input.parent_hash,
            user_transactions = extracted.user_transaction_count,
            checkpoint = ?extracted.checkpoint_number,
            has_finalization = extracted.has_finalization,
            "extracted Zone block"
        );
        blocks.push(extracted);
    }
    Ok((parent_header, parent_number, blocks))
}

async fn fetch_full_block(zone: &DynProvider<TempoNetwork>, number: u64) -> Result<RpcBlock> {
    zone.get_block_by_number(BlockNumberOrTag::Number(number))
        .full()
        .await?
        .ok_or_else(|| eyre!("Zone block {number} not found"))
}

async fn zone_header(zone: &DynProvider<TempoNetwork>, number: u64) -> Result<TempoHeader> {
    let block = zone
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await?
        .ok_or_else(|| eyre!("Zone block {number} not found"))?;
    Ok(block.header.as_ref().clone())
}

fn extract_block(block: RpcBlock) -> Result<ExtractedBlock> {
    let header = block.header.as_ref().clone();
    let transactions = match block.transactions {
        BlockTransactions::Full(transactions) => transactions,
        _ => bail!(
            "Zone block {} did not return full transactions",
            header.number()
        ),
    };

    let mut tempo_header_rlp = None;
    let mut deposits = Vec::new();
    let mut decryptions = Vec::new();
    let mut enabled_tokens = Vec::new();
    let mut finalize_count = None;
    let mut finalize_encrypted_senders = Vec::new();
    let mut user_transactions = Vec::new();
    let mut checkpoint_number = None;

    for transaction in transactions {
        let envelope = transaction.into_inner();
        if !envelope.is_system_tx() {
            user_transactions.push(Bytes::from(envelope.encoded_2718()));
            continue;
        }

        match envelope.to() {
            Some(to) if to == ZONE_INBOX_ADDRESS => {
                if tempo_header_rlp.is_some() {
                    bail!(
                        "Zone block {} contains multiple advanceTempo calls",
                        header.number()
                    );
                }
                let call = ZoneInbox::advanceTempoCall::abi_decode(envelope.input())
                    .wrap_err_with(|| {
                        format!("decode advanceTempo in Zone block {}", header.number())
                    })?;
                checkpoint_number = Some(decode_tempo_header(&call.header)?.number());
                tempo_header_rlp = Some(call.header);
                deposits = call.deposits;
                decryptions = call.decryptions;
                enabled_tokens = call.enabledTokens;
            }
            Some(to) if to == ZONE_OUTBOX_ADDRESS => {
                if finalize_count.is_some() {
                    bail!(
                        "Zone block {} contains multiple finalizeWithdrawalBatch calls",
                        header.number()
                    );
                }
                let call = ZoneOutbox::finalizeWithdrawalBatchCall::abi_decode(envelope.input())
                    .wrap_err_with(|| {
                        format!(
                            "decode finalizeWithdrawalBatch in Zone block {}",
                            header.number()
                        )
                    })?;
                if call.blockNumber != header.number() {
                    bail!(
                        "finalization in Zone block {} declares block {}",
                        header.number(),
                        call.blockNumber
                    );
                }
                finalize_count = Some(call.count);
                finalize_encrypted_senders = call.encryptedSenders;
            }
            other => bail!(
                "unsupported system transaction target {other:?} in Zone block {}",
                header.number()
            ),
        }
    }

    let has_finalization = finalize_count.is_some();
    let user_transaction_count = user_transactions.len();
    Ok(ExtractedBlock {
        input: ZoneBlock {
            number: header.number(),
            parent_hash: header.parent_hash(),
            timestamp: header.timestamp(),
            beneficiary: header.beneficiary(),
            tempo_header_rlp,
            deposits,
            decryptions,
            enabled_tokens,
            finalize_withdrawal_batch_count: finalize_count,
            finalize_withdrawal_batch_encrypted_senders: finalize_encrypted_senders,
            transactions: user_transactions,
        },
        checkpoint_number,
        has_finalization,
        user_transaction_count,
    })
}

async fn initial_tempo_header(
    tempo: &DynProvider<TempoNetwork>,
    zone: &DynProvider<TempoNetwork>,
    parent_number: u64,
) -> Result<TempoHeader> {
    let block_id = BlockId::number(parent_number);
    let (hash_word, number_word) = tokio::try_join!(
        zone.get_storage_at(
            TEMPO_STATE_ADDRESS,
            U256::from(tempo_state_slots::TEMPO_BLOCK_HASH)
        )
        .block_id(block_id),
        zone.get_storage_at(
            TEMPO_STATE_ADDRESS,
            U256::from(tempo_state_slots::TEMPO_BLOCK_NUMBER)
        )
        .block_id(block_id),
    )?;
    let expected_hash = B256::from(hash_word.to_be_bytes::<32>());
    let checkpoint_number = number_word.to::<u64>();
    let header = tempo_header(tempo, checkpoint_number).await?;
    if header.hash_slow() != expected_hash {
        bail!(
            "parent Zone state commits Tempo block {checkpoint_number} hash {expected_hash}, but RPC returned {}",
            header.hash_slow()
        );
    }
    Ok(header)
}

async fn tempo_header(tempo: &DynProvider<TempoNetwork>, number: u64) -> Result<TempoHeader> {
    let block = tempo
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await?
        .ok_or_else(|| eyre!("Tempo block {number} not found"))?;
    Ok(block.header.as_ref().clone())
}

fn decode_tempo_header(encoded: &[u8]) -> Result<TempoHeader> {
    let mut input = encoded;
    let header = alloy_rlp::Decodable::decode(&mut input).context("decode Tempo header RLP")?;
    if !input.is_empty() {
        bail!("Tempo header RLP has trailing bytes");
    }
    Ok(header)
}

async fn zone_witnesses(
    zone: &DynProvider<TempoNetwork>,
    from: u64,
    to: u64,
) -> Result<(ZoneStateWitness, Vec<(u64, Value)>)> {
    let results = stream::iter(from..=to)
        .map(|number| async move {
            let started = Instant::now();
            debug!(zone_block = number, "requesting execution witness and call trace");
            let witness = zone
                .client()
                .request::<_, ExecutionWitness>(
                    "debug_executionWitness",
                    (BlockNumberOrTag::Number(number),),
                )
                .await
                .wrap_err_with(|| format!("debug_executionWitness for Zone block {number}"))?;
            if witness.headers.len() > 1 {
                bail!(
                    "Zone block {number} reads an older BLOCKHASH, which the current SPF witness cannot represent"
                );
            }
            let trace = zone
                .client()
                .request::<_, Value>(
                    "debug_traceBlockByNumber",
                    (
                        BlockNumberOrTag::Number(number),
                        serde_json::json!({"tracer": "callTracer"}),
                    ),
                )
                .await
                .wrap_err_with(|| format!("call trace for Zone block {number}"))?;
            debug!(
                zone_block = number,
                state_nodes = witness.state.len(),
                bytecodes = witness.codes.len(),
                ancestor_headers = witness.headers.len(),
                elapsed_ms = started.elapsed().as_millis(),
                "received execution witness and call trace"
            );
            Ok::<_, eyre::Report>((number, witness, trace))
        })
        .buffer_unordered(RPC_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;

    let mut state = BTreeMap::new();
    let mut codes = BTreeMap::new();
    let mut traces = BTreeMap::new();
    for (number, witness, trace) in results {
        for node in witness.state {
            state.entry(keccak256(&node)).or_insert(node);
        }
        for code in witness.codes {
            codes.entry(keccak256(&code)).or_insert(code);
        }
        traces.insert(number, trace);
    }

    Ok((
        ZoneStateWitness {
            node_pool: state.into_values().collect(),
            bytecodes: codes.into_values().collect(),
        },
        traces.into_iter().collect(),
    ))
}

fn collect_l1_reads(traces: &[(u64, Value)], checkpoints: &BTreeMap<u64, u64>) -> Result<L1Reads> {
    let mut reads = L1Reads::new();
    for (zone_block, trace) in traces {
        let checkpoint = checkpoints
            .get(zone_block)
            .copied()
            .ok_or_else(|| eyre!("missing Tempo checkpoint for Zone block {zone_block}"))?;
        collect_trace_reads(trace, checkpoint, &mut reads)?;
    }
    Ok(reads)
}

fn collect_trace_reads(value: &Value, checkpoint: u64, reads: &mut L1Reads) -> Result<()> {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_trace_reads(value, checkpoint, reads)?;
            }
        }
        Value::Object(object) => {
            if let (Some(to), Some(input)) = (
                object.get("to").and_then(Value::as_str),
                object.get("input").and_then(Value::as_str),
            ) {
                let to: Address = to.parse().wrap_err("parse call-trace target")?;
                if to == TEMPO_STATE_ADDRESS {
                    let input: Bytes = input.parse().wrap_err("parse TempoState trace input")?;
                    if let Ok(call) = TempoState::readTempoStorageSlotCall::abi_decode(&input) {
                        reads
                            .entry(checkpoint)
                            .or_default()
                            .entry(call.account)
                            .or_default()
                            .insert(call.slot);
                    } else if let Ok(call) =
                        TempoState::readTempoStorageSlotsCall::abi_decode(&input)
                    {
                        reads
                            .entry(checkpoint)
                            .or_default()
                            .entry(call.account)
                            .or_default()
                            .extend(call.slots);
                    }
                }
            }
            for value in object.values() {
                collect_trace_reads(value, checkpoint, reads)?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn tempo_state_witness(
    tempo: &DynProvider<TempoNetwork>,
    initial_header: &TempoHeader,
    reads: L1Reads,
) -> Result<TempoStateWitness> {
    let requests = reads
        .into_iter()
        .flat_map(|(block, accounts)| {
            accounts.into_iter().map(move |(account, slots)| {
                (block, account, slots.into_iter().collect::<Vec<_>>())
            })
        })
        .collect::<Vec<_>>();
    let proofs = stream::iter(requests)
        .map(|(block, account, slots)| async move {
            let started = Instant::now();
            debug!(
                tempo_block = block,
                account = %account,
                storage_slots = slots.len(),
                "requesting Tempo state proof"
            );
            let proof = tempo
                .get_proof(account, slots)
                .block_id(BlockId::number(block))
                .await
                .wrap_err_with(|| format!("eth_getProof for {account} at Tempo block {block}"))?;
            debug!(
                tempo_block = block,
                account = %account,
                account_proof_nodes = proof.account_proof.len(),
                storage_proofs = proof.storage_proof.len(),
                elapsed_ms = started.elapsed().as_millis(),
                "received Tempo state proof"
            );
            Ok::<_, eyre::Report>(proof)
        })
        .buffer_unordered(RPC_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;

    let mut nodes = BTreeMap::new();
    for proof in proofs {
        for node in proof.account_proof {
            nodes.entry(keccak256(&node)).or_insert(node);
        }
        for storage in proof.storage_proof {
            for node in storage.proof {
                nodes.entry(keccak256(&node)).or_insert(node);
            }
        }
    }

    Ok(TempoStateWitness {
        initial_tempo_header_rlp: Bytes::from(alloy_rlp::encode(initial_header)),
        node_pool: nodes.into_values().collect(),
    })
}

async fn tempo_anchor(
    tempo: &DynProvider<TempoNetwork>,
    checkpoint: &TempoHeader,
) -> Result<(u64, B256, Vec<Bytes>, &'static str)> {
    let checkpoint_number = checkpoint.number();
    let tip = tempo.get_block_number().await?;
    if checkpoint_number >= tip {
        bail!("Tempo checkpoint {checkpoint_number} is not yet confirmed behind tip {tip}");
    }
    let gap = tip - checkpoint_number;
    if gap < EIP2935_HISTORY_WINDOW - EIP2935_SAFETY_MARGIN {
        return Ok((
            checkpoint_number,
            checkpoint.hash_slow(),
            Vec::new(),
            "direct",
        ));
    }

    let anchor = tip.saturating_sub(EIP2935_SAFETY_MARGIN);
    let mut headers =
        stream::iter(checkpoint_number + 1..=anchor)
            .map(|number| async move {
                Ok::<_, eyre::Report>((number, tempo_header(tempo, number).await?))
            })
            .buffer_unordered(RPC_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;
    headers.sort_unstable_by_key(|(number, _)| *number);
    let anchor_hash = headers
        .last()
        .map(|(_, header)| header.hash_slow())
        .ok_or_else(|| eyre!("empty Tempo ancestry"))?;
    Ok((
        anchor,
        anchor_hash,
        headers
            .into_iter()
            .map(|(_, header)| Bytes::from(alloy_rlp::encode(header)))
            .collect(),
        "ancestry",
    ))
}

fn validate_witness(tempo_chain_id: u64, witness: &BatchWitness) -> Result<BatchOutput> {
    let tempo_spec = chainspec_from_chain_id(tempo_chain_id)
        .ok_or_else(|| eyre!("unsupported Tempo chain ID {tempo_chain_id}"))?;
    let config = SpfConfig::new(Arc::new(ZoneChainSpec::from(tempo_spec)));
    prove_zone_batch(&config, witness.clone()).context("generated witness failed SPF validation")
}

fn print_summary(
    discovery: &Discovery,
    witness: &BatchWitness,
    extracted: &[ExtractedBlock],
    output: &BatchOutput,
    anchor_mode: &str,
    output_path: Option<&PathBuf>,
    output_bytes: Option<usize>,
) {
    let first = witness.zone_blocks.first().expect("non-empty batch");
    let last = witness.zone_blocks.last().expect("non-empty batch");
    let user_transactions = extracted
        .iter()
        .map(|block| block.user_transaction_count)
        .sum::<usize>();
    let zone_node_bytes = witness
        .zone_state_witness
        .node_pool
        .iter()
        .map(|node| node.len())
        .sum::<usize>();
    let tempo_node_bytes = witness
        .tempo_state_witness
        .node_pool
        .iter()
        .map(|node| node.len())
        .sum::<usize>();

    println!("Generated and validated SPF input");
    println!("  Zone ID:               {}", discovery.zone_id);
    println!("  Portal:                {}", discovery.portal);
    println!(
        "  Zone blocks:           {}..={} ({})",
        first.number,
        last.number,
        witness.zone_blocks.len()
    );
    println!(
        "  Withdrawal finalizes:  {}",
        extracted
            .iter()
            .filter(|block| block.has_finalization)
            .count()
    );
    println!("  User transactions:     {user_transactions}");
    println!(
        "  Tempo checkpoint:      {}",
        witness.public_inputs.tempo_block_number
    );
    println!(
        "  Anchor:                {anchor_mode} at {}",
        witness.public_inputs.anchor_block_number
    );
    println!(
        "  Zone witness:          {} nodes / {} code blobs / {} bytes",
        witness.zone_state_witness.node_pool.len(),
        witness.zone_state_witness.bytecodes.len(),
        zone_node_bytes
    );
    println!(
        "  Tempo witness:         {} nodes / {} bytes",
        witness.tempo_state_witness.node_pool.len(),
        tempo_node_bytes
    );
    println!(
        "  Previous block hash:   {}",
        output.block_transition.prevBlockHash
    );
    println!(
        "  Next block hash:       {}",
        output.block_transition.nextBlockHash
    );
    match (output_path, output_bytes) {
        (Some(path), Some(bytes)) => println!(
            "  Output:                {} ({bytes} bytes)",
            path.display()
        ),
        _ => println!("  Output:                not written (pass --output <PATH>)"),
    }
}

fn parse_quantity(value: &str) -> Result<u64> {
    if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).wrap_err_with(|| format!("parse RPC quantity {value}"))
    } else {
        value
            .parse()
            .wrap_err_with(|| format!("parse RPC quantity {value}"))
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{:.3}s", duration.as_secs_f64())
    } else {
        format!("{:.3}ms", duration.as_secs_f64() * 1_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn collects_tempo_storage_reads_from_nested_call_traces() {
        let account = address!("00000000000000000000000000000000000000aa");
        let slot = B256::repeat_byte(0x11);
        let input = TempoState::readTempoStorageSlotCall { account, slot }.abi_encode();
        let trace = serde_json::json!([{
            "result": {
                "to": format!("{TEMPO_STATE_ADDRESS:#x}"),
                "input": format!("0x{}", const_hex::encode(input)),
                "calls": []
            }
        }]);
        let checkpoints = BTreeMap::from([(4_u64, 99_u64)]);

        let reads = collect_l1_reads(&[(4, trace)], &checkpoints).unwrap();

        assert!(reads[&99][&account].contains(&slot));
    }

    #[test]
    fn parses_hex_and_decimal_rpc_quantities() {
        assert_eq!(parse_quantity("0x10").unwrap(), 16);
        assert_eq!(parse_quantity("10").unwrap(), 10);
    }
}
