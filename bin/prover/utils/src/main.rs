use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::{BlockHeader as _, Sealable as _, Transaction as _};
use alloy_eips::{BlockId, eip2718::Encodable2718 as _};
use alloy_genesis::Genesis;
use alloy_network::primitives::BlockTransactions;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_types_eth::{Block, BlockNumberOrTag, Transaction};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall as _;
use clap::{Parser, Subcommand};
use eyre::{Context, OptionExt, Result, bail, eyre};
use futures::{SinkExt, StreamExt, TryStreamExt, stream};
use tempo_alloy::{TempoNetwork, rpc::TempoHeaderResponse};
use tempo_chainspec::{TempoChainSpec, spec::chainspec_from_chain_id};
use tempo_primitives::{TempoHeader, TempoTxEnvelope};
use tempo_zone_contracts::{
    IZoneInbox as ZoneInbox, IZoneOutbox as ZoneOutbox, TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS,
    ZONE_OUTBOX_ADDRESS, ZonePortal,
};
use tempo_zone_prover_enclave::{
    DEFAULT_MAX_REQUEST_BYTES, PROTOCOL_VERSION, VerifyRequest, VerifyResponse, framed,
};
use tokio::net::TcpStream;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;
use zone_chainspec::ZoneChainSpec;
use zone_precompiles::tempo_state::slots as tempo_state_slots;
use zone_primitives::constants::{ZONE_OUTBOX_LAST_BATCH_INDEX_SLOT, zone_chain_id};
use zone_rpc::{
    ZoneProvider, ZoneProviderConfig,
    types::{TempoStorageRead, ZoneExecutionWitness},
};
use zone_spf::{
    BatchOutput, BatchWitness, PublicInputs, SpfConfig, TempoStateWitness, ZoneBlock,
    ZoneStateWitness, prove_zone_batch,
};

const EIP2935_HISTORY_WINDOW: u64 = 8191;
const EIP2935_SAFETY_MARGIN: u64 = 360;
const RPC_CONCURRENCY: usize = 8;
const ZONE_HEAD_POLL_INTERVAL: Duration = Duration::from_secs(1);

type RpcBlock = Block<Transaction<TempoTxEnvelope>, TempoHeaderResponse>;
type L1Reads = BTreeMap<u64, BTreeMap<Address, BTreeSet<B256>>>;

#[derive(Debug, Parser)]
#[command(
    name = "tempo-zone-prover-utils",
    about = "Tempo Zone prover development utilities"
)]
struct Cli {
    /// Tracing filter. Can also be set with RUST_LOG.
    #[arg(
        long,
        global = true,
        env = "RUST_LOG",
        default_value = "tempo_zone_prover_utils=info"
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

    /// Tempo genesis JSON path or HTTP(S) URL. Required for unknown Tempo chain IDs.
    #[arg(long, value_name = "PATH_OR_URL")]
    tempo_genesis: Option<String>,

    /// Authenticated private Zone HTTP RPC URL validated against Zone discovery.
    #[arg(long)]
    zone_private_rpc_url: String,

    /// Unrestricted Zone RPC URL used for full blocks, state, and debug methods.
    #[arg(long)]
    zone_unrestricted_rpc_url: String,

    /// Private key used to authenticate with the private Zone RPC.
    #[arg(long, env = "PRIVATE_KEY", value_name = "HEX", hide_env_values = true)]
    private_key: String,

    /// Override the first Zone block in the batch.
    #[arg(long)]
    from_block: Option<u64>,

    /// Override the final Zone block. Defaults to the current Zone tip.
    #[arg(long, conflicts_with = "zone_block_count")]
    to_block: Option<u64>,

    /// Execute exactly this many Zone blocks, waiting for the target block if necessary.
    #[arg(long, conflicts_with = "to_block")]
    zone_block_count: Option<u64>,

    /// Stop waiting for `--zone-block-count` after this many seconds.
    #[arg(long, value_name = "SECONDS", requires = "zone_block_count")]
    wait_timeout: Option<u64>,

    /// Write the complete JSON witness to this path.
    #[arg(long, short)]
    output: Option<PathBuf>,

    /// Send the generated witness to a Tempo Zone prover TCP socket.
    #[arg(long, value_name = "HOST:PORT")]
    target: Option<String>,
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
    portal_withdrawal_batch_index: u64,
    portal_tempo_block_number: u64,
    tempo_chain_id: u64,
    portal_block_hash: B256,
}

#[derive(Debug)]
struct PortalSnapshot {
    withdrawal_batch_index: u64,
    tempo_block_number: u64,
    block_hash: B256,
}

#[derive(Debug)]
struct ExtractedBlock {
    input: ZoneBlock,
    block_hash: B256,
    checkpoint_number: u64,
    has_finalization: bool,
    user_transaction_count: usize,
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
        from_block = ?args.from_block,
        to_block = ?args.to_block,
        zone_block_count = ?args.zone_block_count,
        wait_timeout_seconds = ?args.wait_timeout,
        writes_output = args.output.is_some(),
        target = ?args.target,
        "generating SPF input"
    );
    if args.zone_block_count == Some(0) {
        bail!("--zone-block-count must be greater than zero");
    }

    let started = start_phase("discovery");
    let tempo_provider = connect(&args.tempo_rpc_url, "Tempo").await?;
    let zone_provider = connect(&args.zone_unrestricted_rpc_url, "unrestricted Zone").await?;
    let signer = args
        .private_key
        .parse::<PrivateKeySigner>()
        .context("parse private Zone RPC key")?;
    let (mut discovery, zone_chain_id) = discover(&tempo_provider, &zone_provider).await?;
    let spf_config = spf_config(
        discovery.tempo_chain_id,
        args.tempo_genesis.as_deref(),
        discovery.portal,
    )
    .await?;
    let private_zone_provider = connect_private_zone(
        &args.zone_private_rpc_url,
        signer,
        discovery.zone_id,
        zone_chain_id,
    )?;
    validate_private_zone(&private_zone_provider, &discovery).await?;
    info!(
        zone_id = discovery.zone_id,
        portal = %discovery.portal,
        committed_zone_hash = %discovery.portal_block_hash,
        portal_tempo_block = discovery.portal_tempo_block_number,
        withdrawal_batch_index = discovery.portal_withdrawal_batch_index,
        "discovered Zone"
    );
    timings.record("discovery", started, ());

    let started = start_phase("batch extraction");
    let (parent_header, parent_number, extracted) = if let Some(block_count) = args.zone_block_count
    {
        let (updated_discovery, parent_header, parent_number, extracted) = discover_counted_batch(
            &zone_provider,
            &tempo_provider,
            discovery,
            args.from_block,
            block_count,
            args.wait_timeout.map(Duration::from_secs),
        )
        .await?;
        discovery = updated_discovery;
        (parent_header, parent_number, extracted)
    } else {
        discover_batch(&zone_provider, &discovery, args.from_block, args.to_block).await?
    };
    let from_block = extracted
        .first()
        .expect("batch discovery returns a non-empty batch")
        .input
        .number;
    let last_extracted = extracted.last().expect("non-empty");
    let to_block = last_extracted.input.number;
    let next_block_hash = last_extracted.block_hash;
    let finalization_count = extracted
        .iter()
        .filter(|block| block.has_finalization)
        .count();
    let parent_withdrawal_batch_index = withdrawal_batch_index_at(&zone_provider, parent_number)
        .await
        .context("read withdrawal batch index from parent Zone state")?;
    if args.from_block.is_none()
        && parent_withdrawal_batch_index != discovery.portal_withdrawal_batch_index
    {
        bail!(
            "parent Zone state has withdrawal batch index {parent_withdrawal_batch_index}, but the Tempo portal reports {}",
            discovery.portal_withdrawal_batch_index,
        );
    }
    let expected_withdrawal_batch_index = parent_withdrawal_batch_index
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
    let (zone_state_witness, tempo_reads) =
        zone_witnesses(&zone_provider, from_block, to_block).await?;
    timings.record("Zone state witness", started, ());

    let started = start_phase("Tempo state witness");
    let checkpoint_by_zone_block = extracted
        .iter()
        .map(|block| (block.input.number, block.checkpoint_number))
        .collect();
    let reads = collect_l1_reads(tempo_reads, &checkpoint_by_zone_block)?;
    let initial_tempo_state_witness =
        tempo_state_witness(&tempo_provider, &initial_tempo_header, reads).await?;
    timings.record("Tempo state witness", started, ());

    let final_tempo_header = extracted
        .iter()
        .last()
        .map(|encoded| decode_tempo_header(&encoded.input.tempo_header_rlp))
        .transpose()?
        .unwrap_or_else(|| initial_tempo_header.clone());

    let started = start_phase("Tempo ancestry");
    let (anchor_block_number, anchor_block_hash, tempo_ancestry_headers, anchor_mode) =
        tempo_anchor(&tempo_provider, &final_tempo_header).await?;
    timings.record("Tempo ancestry", started, ());

    let witness = BatchWitness {
        public_inputs: PublicInputs {
            parent_chain_id: discovery.tempo_chain_id,
            zone_id: discovery.zone_id,
            portal: discovery.portal,
            tempo_block_number: final_tempo_header.number(),
            anchor_block_number,
            anchor_block_hash,
            expected_withdrawal_batch_index,
        },
        parent_header,
        zone_blocks: extracted.iter().map(|block| block.input.clone()).collect(),
        zone_state_witness,
        tempo_state_witness: initial_tempo_state_witness,
        tempo_ancestry_headers,
    };

    let started = start_phase("SPF validation");
    let output = prove_zone_batch(&spf_config, witness.clone())
        .context("generated witness failed SPF validation")?;
    if output.block_transition.nextBlockHash != next_block_hash {
        bail!(
            "SPF replay produced {}, but Zone block {to_block} has hash {next_block_hash}",
            output.block_transition.nextBlockHash
        );
    }
    timings.record("SPF validation", started, ());

    let request_id = format!(
        "zone-{}-{from_block}-{to_block}-{}",
        discovery.zone_id, output.block_transition.nextBlockHash
    );
    let request = VerifyRequest {
        version: PROTOCOL_VERSION,
        request_id,
        tempo_chain_id: discovery.tempo_chain_id,
        witness,
    };

    let started = start_phase("output");
    let output_bytes = if let Some(path) = &args.output {
        let json =
            serde_json::to_vec_pretty(&request.witness).context("serialize batch witness")?;
        std::fs::write(path, &json)
            .wrap_err_with(|| format!("write SPF input to {}", path.display()))?;
        Some(json.len())
    } else {
        None
    };
    timings.record("output", started, ());

    let target_bytes = if let Some(target) = &args.target {
        let started = start_phase("target prover");
        let bytes = send_to_prover(target, &request, &output).await?;
        timings.record("target prover", started, ());
        Some(bytes)
    } else {
        None
    };

    print_summary(
        &discovery,
        &request.witness,
        &extracted,
        &output,
        anchor_mode,
        args.output.as_ref().zip(output_bytes),
        args.target.as_deref().zip(target_bytes),
    );
    timings.print(total_started.elapsed());
    Ok(())
}

async fn send_to_prover(
    target: &str,
    request: &VerifyRequest,
    expected_output: &BatchOutput,
) -> Result<usize> {
    let payload = serde_json::to_vec(request).context("serialize prover request")?;
    let request_bytes = payload.len();
    let stream = TcpStream::connect(target)
        .await
        .wrap_err_with(|| format!("connect to target prover at {target}"))?;
    let mut stream = framed(stream, DEFAULT_MAX_REQUEST_BYTES);
    stream
        .send(payload.into())
        .await
        .wrap_err_with(|| format!("send request to target prover at {target}"))?;
    let response_payload = stream
        .next()
        .await
        .ok_or_else(|| eyre!("target prover closed the connection without a response"))?
        .map_err(|error| eyre!(error))
        .wrap_err_with(|| format!("read response from target prover at {target}"))?;
    let response = serde_json::from_slice::<VerifyResponse>(&response_payload)
        .wrap_err_with(|| format!("decode response from target prover at {target}"))?;

    match response {
        VerifyResponse::Ok {
            version,
            request_id,
            output,
        } => {
            if version != PROTOCOL_VERSION {
                bail!(
                    "target prover responded with protocol version {version}; expected {PROTOCOL_VERSION}"
                );
            }
            if request_id != request.request_id {
                bail!(
                    "target prover response request ID {request_id:?} does not match {:?}",
                    request.request_id
                );
            }
            if output != *expected_output {
                bail!("target prover output does not match local SPF output");
            }
        }
        VerifyResponse::Error {
            version,
            request_id,
            code,
            message,
        } => {
            if version != PROTOCOL_VERSION {
                bail!(
                    "target prover responded with protocol version {version}; expected {PROTOCOL_VERSION}"
                );
            }
            if let Some(response_id) = request_id
                && response_id != request.request_id
            {
                bail!(
                    "target prover error request ID {response_id:?} does not match {:?}",
                    request.request_id
                );
            }
            bail!("target prover rejected request ({code:?}): {message}");
        }
    }

    Ok(request_bytes)
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
    zone: &DynProvider<TempoNetwork>,
) -> Result<(Discovery, u64)> {
    let inbox = ZoneInbox::new(ZONE_INBOX_ADDRESS, zone.clone());
    let portal_call = inbox.tempoPortal();
    let (tempo_chain_id, actual_zone_chain_id, portal_address) = tokio::try_join!(
        async { tempo.get_chain_id().await.context("read Tempo chain ID") },
        async {
            zone.get_chain_id()
                .await
                .context("read unrestricted Zone chain ID")
        },
        async {
            portal_call
                .call()
                .await
                .context("read Tempo portal from unrestricted Zone RPC")
        }
    )?;
    if portal_address == Address::ZERO {
        bail!("ZoneInbox reports a zero Tempo portal address");
    }
    let (zone_id, portal) = tokio::try_join!(
        async {
            ZonePortal::new(portal_address, tempo.clone())
                .zoneId()
                .call()
                .await
                .wrap_err_with(|| format!("read Zone ID from portal {portal_address}"))
        },
        read_portal_snapshot(tempo, portal_address),
    )?;
    let expected_chain_id = zone_chain_id(tempo_chain_id, zone_id)?;
    if actual_zone_chain_id != expected_chain_id {
        bail!(
            "Zone portal reports Zone ID {zone_id}, which requires chain ID {expected_chain_id}, but the unrestricted Zone RPC reports {actual_zone_chain_id}"
        );
    }

    Ok((
        Discovery {
            zone_id,
            portal: portal_address,
            portal_withdrawal_batch_index: portal.withdrawal_batch_index,
            portal_tempo_block_number: portal.tempo_block_number,
            tempo_chain_id,
            portal_block_hash: portal.block_hash,
        },
        actual_zone_chain_id,
    ))
}

async fn read_portal_snapshot(
    tempo: &DynProvider<TempoNetwork>,
    portal_address: Address,
) -> Result<PortalSnapshot> {
    let portal = ZonePortal::new(portal_address, tempo.clone());
    let withdrawal_batch_index_call = portal.withdrawalBatchIndex();
    let block_hash_call = portal.blockHash();
    let tempo_block_number_call = portal.lastSyncedTempoBlockNumber();
    let (withdrawal_batch_index, block_hash, tempo_block_number) = tokio::try_join!(
        withdrawal_batch_index_call.call(),
        block_hash_call.call(),
        tempo_block_number_call.call(),
    )
    .wrap_err_with(|| format!("read Zone portal state from {portal_address}"))?;
    Ok(PortalSnapshot {
        withdrawal_batch_index,
        tempo_block_number,
        block_hash,
    })
}

fn apply_portal_snapshot(discovery: &mut Discovery, snapshot: PortalSnapshot) {
    discovery.portal_withdrawal_batch_index = snapshot.withdrawal_batch_index;
    discovery.portal_tempo_block_number = snapshot.tempo_block_number;
    discovery.portal_block_hash = snapshot.block_hash;
}

async fn validate_private_zone(
    private_zone: &DynProvider<TempoNetwork>,
    discovery: &Discovery,
) -> Result<()> {
    // This first request authenticates with the discovered, fully scoped Zone
    // and chain IDs before checking that both RPC endpoints expose the same Zone.
    let inbox = ZoneInbox::new(ZONE_INBOX_ADDRESS, private_zone.clone());
    let portal = inbox
        .tempoPortal()
        .call()
        .await
        .context("read Tempo portal from private Zone RPC")?;
    if portal != discovery.portal {
        bail!(
            "private Zone RPC points to Tempo portal {portal}, but the unrestricted Zone RPC points to {}",
            discovery.portal,
        );
    }
    Ok(())
}

async fn discover_counted_batch(
    zone: &DynProvider<TempoNetwork>,
    tempo: &DynProvider<TempoNetwork>,
    mut discovery: Discovery,
    from_override: Option<u64>,
    block_count: u64,
    wait_timeout: Option<Duration>,
) -> Result<(Discovery, TempoHeader, u64, Vec<ExtractedBlock>)> {
    let wait_started = Instant::now();

    'selection: loop {
        let from = match from_override {
            Some(from) => from,
            None => portal_parent_number(zone, &discovery)
                .await?
                .checked_add(1)
                .ok_or_else(|| eyre!("Zone block number overflow after the portal commitment"))?,
        };
        let (_, to) = counted_range(from, block_count)?;
        info!(
            from_block = from,
            to_block = to,
            block_count,
            committed_zone_hash = %discovery.portal_block_hash,
            "waiting for counted Zone block range"
        );

        let mut last_logged_head = None;
        loop {
            let zone_head = if from_override.is_none() {
                let portal = ZonePortal::new(discovery.portal, tempo.clone());
                let block_hash_call = portal.blockHash();
                let (zone_head, portal_block_hash) =
                    tokio::join!(zone.get_block_number(), block_hash_call.call());
                let zone_head = zone_head.context("read Zone head while waiting")?;
                let portal_block_hash =
                    portal_block_hash.context("read portal block hash while waiting")?;
                if portal_block_hash != discovery.portal_block_hash {
                    info!(
                        old_committed_zone_hash = %discovery.portal_block_hash,
                        new_committed_zone_hash = %portal_block_hash,
                        "portal advanced while waiting; rebasing counted range"
                    );
                    let snapshot = read_portal_snapshot(tempo, discovery.portal).await?;
                    apply_portal_snapshot(&mut discovery, snapshot);
                    continue 'selection;
                }
                zone_head
            } else {
                zone.get_block_number().await?
            };

            if zone_head >= to {
                info!(
                    zone_head,
                    target_zone_block = to,
                    waited_ms = wait_started.elapsed().as_millis(),
                    "counted Zone block range is available"
                );
                break;
            }

            if last_logged_head != Some(zone_head) {
                info!(
                    zone_head,
                    target_zone_block = to,
                    remaining_blocks = to - zone_head,
                    waited_ms = wait_started.elapsed().as_millis(),
                    "waiting for Zone head"
                );
                last_logged_head = Some(zone_head);
            }

            let sleep_for = match wait_timeout {
                Some(timeout) => {
                    let elapsed = wait_started.elapsed();
                    if elapsed >= timeout {
                        bail!(
                            "timed out after {:.3}s waiting for Zone block {to}; current head is {zone_head}",
                            elapsed.as_secs_f64()
                        );
                    }
                    ZONE_HEAD_POLL_INTERVAL.min(timeout - elapsed)
                }
                None => ZONE_HEAD_POLL_INTERVAL,
            };
            tokio::time::sleep(sleep_for).await;
        }

        if from_override.is_none() {
            let snapshot = read_portal_snapshot(tempo, discovery.portal).await?;
            if snapshot.block_hash != discovery.portal_block_hash {
                info!(
                    old_committed_zone_hash = %discovery.portal_block_hash,
                    new_committed_zone_hash = %snapshot.block_hash,
                    "portal advanced after Zone target was reached; rebasing counted range"
                );
                apply_portal_snapshot(&mut discovery, snapshot);
                continue;
            }
            apply_portal_snapshot(&mut discovery, snapshot);
        }

        let (parent_header, parent_number, blocks) =
            discover_batch(zone, &discovery, from_override, Some(to)).await?;

        if from_override.is_none() {
            let snapshot = read_portal_snapshot(tempo, discovery.portal).await?;
            if snapshot.block_hash != discovery.portal_block_hash {
                info!(
                    old_committed_zone_hash = %discovery.portal_block_hash,
                    new_committed_zone_hash = %snapshot.block_hash,
                    "portal advanced during batch extraction; discarding range and rebasing"
                );
                apply_portal_snapshot(&mut discovery, snapshot);
                continue;
            }
            apply_portal_snapshot(&mut discovery, snapshot);
        }

        return Ok((discovery, parent_header, parent_number, blocks));
    }
}

fn counted_range(from: u64, block_count: u64) -> Result<(u64, u64)> {
    if from == 0 {
        bail!("a batch cannot start at Zone genesis block 0");
    }
    if block_count == 0 {
        bail!("--zone-block-count must be greater than zero");
    }
    let to = from
        .checked_add(block_count - 1)
        .ok_or_else(|| eyre!("Zone block range overflow"))?;
    Ok((from, to))
}

async fn portal_parent_number(
    zone: &DynProvider<TempoNetwork>,
    discovery: &Discovery,
) -> Result<u64> {
    match zone.get_block_by_hash(discovery.portal_block_hash).await? {
        Some(block) => Ok(block.header.number()),
        None => {
            let tip = zone.get_block_number().await?;
            let genesis_hash = zone_header(zone, 0).await?.hash_slow();
            bail!(
                "Tempo portal {} commits Zone block hash {}, but the unrestricted Zone RPC does not contain it (Zone tip: {tip}, genesis hash: {genesis_hash}); the endpoint may belong to a reset or different Zone chain",
                discovery.portal,
                discovery.portal_block_hash
            );
        }
    }
}

async fn discover_batch(
    zone: &DynProvider<TempoNetwork>,
    discovery: &Discovery,
    from_override: Option<u64>,
    to_override: Option<u64>,
) -> Result<(TempoHeader, u64, Vec<ExtractedBlock>)> {
    let portal_parent_number = portal_parent_number(zone, discovery).await?;
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
    let block_hash = header.hash_slow();
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

    let (tempo_header_rlp, checkpoint_number) =
        tempo_header_rlp.zip(checkpoint_number).ok_or_eyre(format!(
            "no advanceTempo call in Zone block {}",
            header.number()
        ))?;

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
        block_hash,
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

async fn withdrawal_batch_index_at(
    zone: &DynProvider<TempoNetwork>,
    block_number: u64,
) -> Result<u64> {
    let index = zone
        .get_storage_at(ZONE_OUTBOX_ADDRESS, ZONE_OUTBOX_LAST_BATCH_INDEX_SLOT)
        .block_id(BlockId::number(block_number))
        .await?;
    Ok(index.as_limbs()[0])
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
) -> Result<(ZoneStateWitness, Vec<(u64, TempoStorageRead)>)> {
    let results = stream::iter(from..=to)
        .map(|number| async move {
            let started = Instant::now();
            debug!(zone_block = number, "requesting Zone execution witness");
            let witness = zone
                .client()
                .request::<_, ZoneExecutionWitness>(
                    "debug_zoneExecutionWitness",
                    (BlockNumberOrTag::Number(number),),
                )
                .await
                .wrap_err_with(|| format!("debug_zoneExecutionWitness for Zone block {number}"))?;
            debug!(
                zone_block = number,
                state_nodes = witness.execution_witness.state.len(),
                bytecodes = witness.execution_witness.codes.len(),
                ancestor_headers = witness.execution_witness.headers.len(),
                tempo_storage_reads = witness.tempo_reads.len(),
                elapsed_ms = started.elapsed().as_millis(),
                "received Zone execution witness"
            );
            Ok::<_, eyre::Report>((number, witness))
        })
        .buffer_unordered(RPC_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;

    let mut state = BTreeMap::new();
    let mut codes = BTreeMap::new();
    let mut tempo_reads = Vec::new();
    for (number, witness) in results {
        for node in witness.execution_witness.state {
            state.entry(keccak256(&node)).or_insert(node);
        }
        for code in witness.execution_witness.codes {
            codes.entry(keccak256(&code)).or_insert(code);
        }
        tempo_reads.extend(witness.tempo_reads.into_iter().map(|read| (number, read)));
    }

    Ok((
        ZoneStateWitness {
            node_pool: state.into_values().collect(),
            bytecodes: codes.into_values().collect(),
        },
        tempo_reads,
    ))
}

fn collect_l1_reads(
    tempo_reads: Vec<(u64, TempoStorageRead)>,
    checkpoints: &BTreeMap<u64, u64>,
) -> Result<L1Reads> {
    let mut reads = L1Reads::new();
    for (zone_block, read) in tempo_reads {
        let checkpoint = checkpoints
            .get(&zone_block)
            .copied()
            .ok_or_else(|| eyre!("missing Tempo checkpoint for Zone block {zone_block}"))?;
        reads
            .entry(checkpoint)
            .or_default()
            .entry(read.account)
            .or_default()
            .insert(read.slot);
    }
    Ok(reads)
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
    if checkpoint_number > tip {
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

async fn spf_config(
    tempo_chain_id: u64,
    genesis_source: Option<&str>,
    portal: Address,
) -> Result<SpfConfig> {
    let tempo_spec = match genesis_source {
        Some(source) => {
            let raw = read_genesis(source).await?;
            let genesis: Genesis =
                serde_json::from_slice(&raw).context("parse Tempo genesis JSON")?;
            if genesis.config.chain_id != tempo_chain_id {
                bail!(
                    "Tempo genesis chain ID {} does not match RPC chain ID {tempo_chain_id}",
                    genesis.config.chain_id
                );
            }
            Arc::new(TempoChainSpec::from_genesis(genesis))
        }
        None => chainspec_from_chain_id(tempo_chain_id).ok_or_else(|| {
            eyre!("unsupported Tempo chain ID {tempo_chain_id}; pass --tempo-genesis <PATH_OR_URL>")
        })?,
    };

    let zone_chain_spec =
        ZoneChainSpec::from(tempo_spec.clone()).with_tempo_hardforks_from(tempo_spec.as_ref());
    Ok(SpfConfig::new(Arc::new(zone_chain_spec), portal))
}

async fn read_genesis(source: &str) -> Result<Vec<u8>> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let response = reqwest::get(source)
            .await
            .context("fetch Tempo genesis URL")?
            .error_for_status()
            .context("Tempo genesis URL returned an error")?;
        return response
            .bytes()
            .await
            .context("read Tempo genesis URL response")
            .map(|bytes| bytes.to_vec());
    }

    tokio::fs::read(source)
        .await
        .wrap_err_with(|| format!("read Tempo genesis file {source}"))
}

fn print_summary(
    discovery: &Discovery,
    witness: &BatchWitness,
    extracted: &[ExtractedBlock],
    output: &BatchOutput,
    anchor_mode: &str,
    written_output: Option<(&PathBuf, usize)>,
    verified_target: Option<(&str, usize)>,
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
    match written_output {
        Some((path, bytes)) => println!(
            "  Output:                {} ({bytes} bytes)",
            path.display()
        ),
        _ => println!("  Output:                not written (pass --output <PATH>)"),
    }
    match verified_target {
        Some((target, bytes)) => {
            println!("  Target prover:         {target} ({bytes} request bytes, verified)")
        }
        _ => println!("  Target prover:         not sent (pass --target <HOST:PORT>)"),
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
    fn groups_tempo_storage_reads_by_checkpoint() {
        let account = address!("00000000000000000000000000000000000000aa");
        let slot = B256::repeat_byte(0x11);
        let checkpoints = BTreeMap::from([(4_u64, 99_u64)]);

        let reads =
            collect_l1_reads(vec![(4, TempoStorageRead { account, slot })], &checkpoints).unwrap();

        assert!(reads[&99][&account].contains(&slot));
    }

    #[test]
    fn derives_an_exact_counted_zone_range() {
        assert_eq!(counted_range(501, 20).unwrap(), (501, 520));
        assert!(counted_range(0, 20).is_err());
        assert!(counted_range(501, 0).is_err());
        assert!(counted_range(u64::MAX, 2).is_err());
    }

    #[tokio::test]
    async fn loads_a_custom_tempo_genesis_from_a_local_path() {
        let mut genesis = Genesis::default();
        genesis.config.chain_id = 31_318;
        let path = std::env::temp_dir().join(format!(
            "tempo-zone-prover-utils-genesis-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, serde_json::to_vec(&genesis).unwrap()).unwrap();

        let config = spf_config(31_318, path.to_str(), Address::ZERO)
            .await
            .unwrap();
        let mismatch = spf_config(31_319, path.to_str(), Address::ZERO)
            .await
            .unwrap_err();

        std::fs::remove_file(path).unwrap();
        assert_eq!(
            config.chain_spec().inner.inner.genesis().config.chain_id,
            31_318
        );
        assert!(
            mismatch
                .to_string()
                .contains("does not match RPC chain ID 31319")
        );
    }
}
