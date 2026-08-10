//! Detached, observational SPF validation for finalized batch candidates.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
    time::Instant,
};

use alloy_consensus::{BlockHeader as _, Sealable as _, Transaction as _};
use alloy_eips::{BlockId, eip2718::Encodable2718 as _};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_provider::{DynProvider, Provider as _};
use alloy_rlp::Decodable as _;
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_sol_types::SolCall as _;
use eyre::{Context as _, OptionExt as _, Result, bail, ensure};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use reth_primitives_traits::RecoveredBlock;
use tempo_alloy::TempoNetwork;
use tempo_primitives::{Block, TempoHeader};
use tempo_zone_contracts::{
    IZoneInbox as ZoneInbox, IZoneOutbox as ZoneOutbox, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS,
};
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::{debug, error, info};
use zone_chainspec::ZoneChainSpec;
use zone_l1::TempoStateExt as _;
use zone_rpc::{ZoneDebugApi, types::TempoStorageRead};
use zone_spf::{
    BatchOutput, BatchWitness, PublicInputs, SpfConfig, TempoStateWitness, ZoneBlock,
    ZoneStateWitness, prove_zone_batch,
};

use crate::{BatchAnchorConfig, BatchData, ZoneSequencerProvider, metrics::ProverMetrics};

/// Number of candidates allowed to wait behind the active validation.
const SHADOW_PROVER_QUEUE_CAPACITY: usize = 2;
const RPC_CONCURRENCY: usize = 8;

type L1Reads = BTreeMap<u64, BTreeMap<Address, BTreeSet<B256>>>;

/// Node-owned inputs required to validate canonical Zone blocks with the SPF.
#[derive(Clone)]
pub struct ShadowProverConfig {
    /// Zone identifier bound into SPF public inputs.
    pub zone_id: u32,
    /// Chain spec used to configure the SPF.
    pub chain_spec: Arc<ZoneChainSpec>,
    /// In-process Zone debug API used to generate execution witnesses.
    pub debug_api: Arc<dyn ZoneDebugApi>,
}

impl fmt::Debug for ShadowProverConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShadowProverConfig")
            .field("zone_id", &self.zone_id)
            .field("chain_spec", &self.chain_spec)
            .field("debug_api", &"<in-process>")
            .finish()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ShadowProver {
    sender: mpsc::Sender<ProverJob>,
}

#[derive(Debug)]
struct ProverJob {
    from: u64,
    to: u64,
    batch: BatchData,
}

#[derive(Debug)]
struct ValidationStats {
    witness_bytes: usize,
    zone_state_nodes: usize,
    tempo_state_nodes: usize,
}

struct ProverContext<P> {
    config: ShadowProverConfig,
    portal: Address,
    anchor_config: BatchAnchorConfig,
    zone_provider: P,
    l1_provider: DynProvider<TempoNetwork>,
}

struct ZoneInputs {
    parent_header: TempoHeader,
    blocks: Vec<ZoneBlock>,
    checkpoint_by_zone_block: BTreeMap<u64, u64>,
    initial_tempo_number: u64,
    initial_tempo_hash: B256,
}

struct Anchor {
    number: u64,
    hash: B256,
    ancestry_headers: Vec<Bytes>,
}

pub(crate) fn spawn_shadow_prover<P: ZoneSequencerProvider>(
    config: ShadowProverConfig,
    portal: Address,
    anchor_config: BatchAnchorConfig,
    zone_provider: P,
    l1_provider: DynProvider<TempoNetwork>,
) -> ShadowProver {
    info!(
        target: "zone::sequencer::prover",
        zone_id = config.zone_id,
        queue_capacity = SHADOW_PROVER_QUEUE_CAPACITY,
        "Shadow prover enabled"
    );
    let (sender, mut receiver) = mpsc::channel(SHADOW_PROVER_QUEUE_CAPACITY);
    let context = ProverContext {
        config,
        portal,
        anchor_config,
        zone_provider,
        l1_provider,
    };
    let metrics = ProverMetrics::default();

    tokio::spawn(async move {
        while let Some(job) = receiver.recv().await {
            let started = Instant::now();
            let result = validate_candidate(&context, &job).await;
            metrics
                .validation_duration_seconds
                .record(started.elapsed().as_secs_f64());
            match result {
                Ok(stats) => {
                    metrics.validation_success_total.increment(1);
                    metrics.witness_bytes.record(stats.witness_bytes as f64);
                    metrics
                        .zone_state_nodes
                        .record(stats.zone_state_nodes as f64);
                    metrics
                        .tempo_state_nodes
                        .record(stats.tempo_state_nodes as f64);
                    info!(
                        target: "zone::sequencer::prover",
                        zone_from = job.from,
                        zone_to = job.to,
                        prev_block_hash = %job.batch.prev_block_hash,
                        next_block_hash = %job.batch.next_block_hash,
                        elapsed_ms = started.elapsed().as_millis(),
                        witness_bytes = stats.witness_bytes,
                        zone_state_nodes = stats.zone_state_nodes,
                        tempo_state_nodes = stats.tempo_state_nodes,
                        "Shadow prover validated finalized batch candidate"
                    );
                }
                Err(err) => {
                    metrics.validation_failure_total.increment(1);
                    error!(
                        target: "zone::sequencer::prover",
                        zone_from = job.from,
                        zone_to = job.to,
                        prev_block_hash = %job.batch.prev_block_hash,
                        next_block_hash = %job.batch.next_block_hash,
                        elapsed_ms = started.elapsed().as_millis(),
                        error = ?err,
                        "Shadow prover failed to validate finalized batch candidate"
                    );
                }
            }
        }
    });

    ShadowProver { sender }
}

impl ShadowProver {
    /// Queue a candidate without waiting for validation or queue capacity.
    pub(crate) fn try_enqueue(&self, from: u64, to: u64, batch: BatchData) {
        if let Err(err) = self.sender.try_send(ProverJob {
            from,
            to,
            batch: batch.clone(),
        }) {
            error!(
                target: "zone::sequencer::prover",
                zone_from = from,
                zone_to = to,
                prev_block_hash = %batch.prev_block_hash,
                next_block_hash = %batch.next_block_hash,
                error = %err,
                "Shadow prover queue {}; skipping finalized batch candidate",
                match err {
                    TrySendError::Full(_) => "full",
                    TrySendError::Closed(_) => "unavailable",
                },
            );
        }
    }
}

async fn validate_candidate<P: ZoneSequencerProvider>(
    context: &ProverContext<P>,
    job: &ProverJob,
) -> Result<ValidationStats> {
    ensure!(
        job.batch.zone_height == job.to,
        "candidate zone height {} does not match range end {}",
        job.batch.zone_height,
        job.to
    );
    ensure!(
        job.from != 1 || job.batch.prev_block_hash.is_zero(),
        "first Zone batch must use the zero portal prev_block_hash sentinel, found {}",
        job.batch.prev_block_hash
    );
    let zone_provider = context.zone_provider.clone();
    let from = job.from;
    let to = job.to;
    let expected_prev_hash = job.batch.prev_block_hash;
    let expected_next_hash = job.batch.next_block_hash;
    let zone_inputs = tokio::task::spawn_blocking(move || {
        build_zone_inputs(
            &zone_provider,
            from,
            to,
            expected_prev_hash,
            expected_next_hash,
        )
    })
    .await
    .context("Zone input worker panicked")??;

    let (zone_state_witness, tempo_reads) =
        zone_witnesses(context.config.debug_api.as_ref(), from, to).await?;

    let initial_tempo_header = tempo_header(&context.l1_provider, zone_inputs.initial_tempo_number)
        .await
        .context("fetch initial Tempo checkpoint")?;
    ensure!(
        initial_tempo_header.hash_slow() == zone_inputs.initial_tempo_hash,
        "parent Zone state commits Tempo block {} hash {}, but L1 returned {}",
        zone_inputs.initial_tempo_number,
        zone_inputs.initial_tempo_hash,
        initial_tempo_header.hash_slow()
    );

    let final_tempo_header = zone_inputs
        .blocks
        .last()
        .map(|block| decode_tempo_header(&block.tempo_header_rlp))
        .transpose()?
        .unwrap_or_else(|| initial_tempo_header.clone());
    ensure!(
        final_tempo_header.number() == job.batch.tempo_block_number,
        "candidate Tempo block is {}, but the final advanceTempo header is {}",
        job.batch.tempo_block_number,
        final_tempo_header.number()
    );

    let anchor = resolve_anchor(
        &context.l1_provider,
        final_tempo_header.number(),
        final_tempo_header.hash_slow(),
        context.anchor_config,
    )
    .await?;

    let reads = collect_l1_reads(tempo_reads, &zone_inputs.checkpoint_by_zone_block)?;
    let tempo_state_witness =
        tempo_state_witness(&context.l1_provider, &initial_tempo_header, reads).await?;

    let witness = BatchWitness {
        public_inputs: PublicInputs {
            zone_id: context.config.zone_id,
            portal: context.portal,
            tempo_block_number: final_tempo_header.number(),
            anchor_block_number: anchor.number,
            anchor_block_hash: anchor.hash,
            expected_withdrawal_batch_index: job.batch.withdrawal_batch_index,
        },
        parent_header: zone_inputs.parent_header,
        zone_blocks: zone_inputs.blocks,
        zone_state_witness,
        tempo_state_witness,
        tempo_ancestry_headers: anchor.ancestry_headers,
    };

    let spf_config = SpfConfig::new(context.config.chain_spec.clone(), context.portal);
    let attempt = witness.clone();
    let output = tokio::task::spawn_blocking(move || prove_zone_batch(&spf_config, attempt))
        .await
        .context("SPF worker panicked")?
        .context("SPF rejected generated witness")?;

    compare_output(&output, &job.batch, witness.parent_header.hash_slow())?;

    Ok(ValidationStats {
        witness_bytes: witness_size(&witness),
        zone_state_nodes: witness.zone_state_witness.node_pool.len(),
        tempo_state_nodes: witness.tempo_state_witness.node_pool.len(),
    })
}

fn build_zone_inputs<P: ZoneSequencerProvider>(
    provider: &P,
    from: u64,
    to: u64,
    expected_prev_hash: B256,
    expected_next_hash: B256,
) -> Result<ZoneInputs> {
    ensure!(from > 0, "SPF batch cannot start at Zone genesis");
    ensure!(from <= to, "invalid Zone batch range {from}..={to}");

    let parent_header = provider
        .header_by_number(from - 1)?
        .ok_or_eyre(format!("canonical Zone parent {} not found", from - 1))?;
    let parent_hash = parent_header.hash_slow();
    // ZonePortal uses zero as its pre-genesis sentinel. SPF receives the real
    // canonical hash of block 0 as the parent of block 1 instead.
    if from == 1 {
        ensure!(
            expected_prev_hash.is_zero(),
            "first Zone batch must use the zero portal prev_block_hash sentinel, found {expected_prev_hash}"
        );
        ensure!(
            !parent_hash.is_zero(),
            "canonical Zone block 0 hash must be non-zero"
        );
    } else {
        ensure!(
            parent_hash == expected_prev_hash,
            "candidate parent hash changed: expected {expected_prev_hash}, found {parent_hash}"
        );
    }
    let parent_state = provider.state_by_block_hash(parent_hash)?;
    let initial_tempo = parent_state.tempo_num_hash()?;

    let recovered = provider.recovered_block_range(from..=to)?;
    ensure!(
        recovered.len() == (to - from + 1) as usize,
        "canonical Zone range {from}..={to} returned {} blocks",
        recovered.len()
    );

    let mut expected_parent = parent_hash;
    let mut extracted = Vec::with_capacity(recovered.len());
    let mut checkpoint_by_zone_block = BTreeMap::new();
    for (offset, block) in recovered.iter().enumerate() {
        let number = from + offset as u64;
        ensure!(
            block.number() == number,
            "canonical Zone range returned block {} at expected height {number}",
            block.number()
        );
        ensure!(
            block.parent_hash() == expected_parent,
            "Zone block {number} parent changed: expected {expected_parent}, found {}",
            block.parent_hash()
        );
        let canonical_hash = provider
            .block_hash(number)?
            .ok_or_eyre(format!("canonical Zone block {number} has no hash"))?;
        ensure!(
            block.hash() == canonical_hash,
            "recovered Zone block {number} is not canonical"
        );

        let extracted_block = extract_zone_block(block)?;
        let checkpoint = decode_tempo_header(&extracted_block.tempo_header_rlp)?.number();
        checkpoint_by_zone_block.insert(number, checkpoint);
        extracted.push(extracted_block);
        expected_parent = canonical_hash;
    }
    ensure!(
        expected_parent == expected_next_hash,
        "candidate tip hash changed: expected {expected_next_hash}, found {expected_parent}"
    );

    Ok(ZoneInputs {
        parent_header,
        blocks: extracted,
        checkpoint_by_zone_block,
        initial_tempo_number: initial_tempo.number,
        initial_tempo_hash: initial_tempo.hash,
    })
}

fn extract_zone_block(block: &RecoveredBlock<Block>) -> Result<ZoneBlock> {
    let header = block.header();
    let mut tempo_header_rlp = None;
    let mut deposits = Vec::new();
    let mut decryptions = Vec::new();
    let mut enabled_tokens = Vec::new();
    let mut finalize_count = None;
    let mut finalize_encrypted_senders = Vec::new();
    let mut user_transactions = Vec::new();

    for transaction in &block.body().transactions {
        if !transaction.is_system_tx() {
            user_transactions.push(Bytes::from(transaction.encoded_2718()));
            continue;
        }

        match transaction.to() {
            Some(to) if to == ZONE_INBOX_ADDRESS => {
                ensure!(
                    tempo_header_rlp.is_none(),
                    "Zone block {} contains multiple advanceTempo calls",
                    header.number()
                );
                let call = ZoneInbox::advanceTempoCall::abi_decode(transaction.input())
                    .wrap_err_with(|| {
                        format!("decode advanceTempo in Zone block {}", header.number())
                    })?;
                decode_tempo_header(&call.header).wrap_err_with(|| {
                    format!("decode Tempo checkpoint in Zone block {}", header.number())
                })?;
                tempo_header_rlp = Some(call.header);
                deposits = call.deposits;
                decryptions = call.decryptions;
                enabled_tokens = call.enabledTokens;
            }
            Some(to) if to == ZONE_OUTBOX_ADDRESS => {
                ensure!(
                    finalize_count.is_none(),
                    "Zone block {} contains multiple finalizeWithdrawalBatch calls",
                    header.number()
                );
                let call = ZoneOutbox::finalizeWithdrawalBatchCall::abi_decode(transaction.input())
                    .wrap_err_with(|| {
                        format!(
                            "decode finalizeWithdrawalBatch in Zone block {}",
                            header.number()
                        )
                    })?;
                ensure!(
                    call.blockNumber == header.number(),
                    "finalization in Zone block {} declares block {}",
                    header.number(),
                    call.blockNumber
                );
                finalize_count = Some(call.count);
                finalize_encrypted_senders = call.encryptedSenders;
            }
            target => bail!(
                "unsupported system transaction target {target:?} in Zone block {}",
                header.number()
            ),
        }
    }

    let tempo_header_rlp = tempo_header_rlp.ok_or_eyre(format!(
        "no advanceTempo call in Zone block {}",
        header.number()
    ))?;

    Ok(ZoneBlock {
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
    })
}

fn decode_tempo_header(encoded: &[u8]) -> Result<TempoHeader> {
    let mut input = encoded;
    let header = TempoHeader::decode(&mut input).context("decode Tempo header RLP")?;
    ensure!(input.is_empty(), "Tempo header RLP has trailing bytes");
    Ok(header)
}

async fn zone_witnesses(
    debug_api: &dyn ZoneDebugApi,
    from: u64,
    to: u64,
) -> Result<(ZoneStateWitness, Vec<(u64, TempoStorageRead)>)> {
    let results = stream::iter(from..=to)
        .map(|number| async move {
            let started = Instant::now();
            debug!(
                target: "zone::sequencer::prover",
                zone_block = number,
                "Requesting Zone execution witness"
            );
            let witness = debug_api
                .zone_execution_witness(BlockNumberOrTag::Number(number))
                .await
                .map_err(|error| eyre::eyre!(error.to_string()))
                .wrap_err_with(|| {
                    format!("debug_zoneExecutionWitness for Zone block {number}")
                })?;
            if witness.execution_witness.headers.len() > 1 {
                bail!(
                    "Zone block {number} reads an older BLOCKHASH, which the current SPF witness cannot represent"
                );
            }
            debug!(
                target: "zone::sequencer::prover",
                zone_block = number,
                state_nodes = witness.execution_witness.state.len(),
                bytecodes = witness.execution_witness.codes.len(),
                ancestor_headers = witness.execution_witness.headers.len(),
                tempo_storage_reads = witness.tempo_reads.len(),
                elapsed_ms = started.elapsed().as_millis(),
                "Received Zone execution witness"
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
        let checkpoint = checkpoints.get(&zone_block).copied().ok_or_eyre(format!(
            "missing Tempo checkpoint for Zone block {zone_block}"
        ))?;
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
    provider: &DynProvider<TempoNetwork>,
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
            provider
                .get_proof(account, slots)
                .block_id(BlockId::number(block))
                .await
                .wrap_err_with(|| format!("eth_getProof for {account} at Tempo block {block}"))
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

async fn tempo_header(provider: &DynProvider<TempoNetwork>, number: u64) -> Result<TempoHeader> {
    provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await?
        .map(|block| block.header.as_ref().clone())
        .ok_or_eyre(format!("Tempo block {number} not found"))
}

async fn resolve_anchor(
    provider: &DynProvider<TempoNetwork>,
    checkpoint_number: u64,
    checkpoint_hash: B256,
    config: BatchAnchorConfig,
) -> Result<Anchor> {
    let tip = provider.get_block_number().await?;
    ensure!(
        checkpoint_number <= tip,
        "Tempo checkpoint {checkpoint_number} is not yet confirmed behind tip {tip}"
    );
    let gap = tip - checkpoint_number;
    if gap < config.effective_window() {
        return Ok(Anchor {
            number: checkpoint_number,
            hash: checkpoint_hash,
            ancestry_headers: Vec::new(),
        });
    }

    let anchor_number = tip.saturating_sub(config.safety_margin());
    ensure!(
        anchor_number > checkpoint_number,
        "Tempo ancestry anchor {anchor_number} does not follow checkpoint {checkpoint_number}"
    );
    let mut expected_parent = checkpoint_hash;
    let mut ancestry_headers = Vec::with_capacity((anchor_number - checkpoint_number) as usize);
    for number in checkpoint_number + 1..=anchor_number {
        let header = tempo_header(provider, number).await?;
        ensure!(
            header.parent_hash() == expected_parent,
            "Tempo ancestry broke at block {number}: expected parent {expected_parent}, found {}",
            header.parent_hash()
        );
        expected_parent = header.hash_slow();
        ancestry_headers.push(Bytes::from(alloy_rlp::encode(&header)));
    }
    Ok(Anchor {
        number: anchor_number,
        hash: expected_parent,
        ancestry_headers,
    })
}

fn compare_output(output: &BatchOutput, batch: &BatchData, expected_prev_hash: B256) -> Result<()> {
    ensure!(
        output.block_transition.prevBlockHash == expected_prev_hash,
        "previous Zone block commitment mismatch: SPF {}, canonical parent {}",
        output.block_transition.prevBlockHash,
        expected_prev_hash
    );
    ensure!(
        output.block_transition.nextBlockHash == batch.next_block_hash,
        "next Zone block commitment mismatch: SPF {}, candidate {}",
        output.block_transition.nextBlockHash,
        batch.next_block_hash
    );
    ensure!(
        output.deposit_queue_transition.prevProcessedHash == batch.prev_processed_deposit_hash,
        "previous deposit hash mismatch: SPF {}, candidate {}",
        output.deposit_queue_transition.prevProcessedHash,
        batch.prev_processed_deposit_hash
    );
    ensure!(
        output.deposit_queue_transition.nextProcessedHash == batch.next_processed_deposit_hash,
        "next deposit hash mismatch: SPF {}, candidate {}",
        output.deposit_queue_transition.nextProcessedHash,
        batch.next_processed_deposit_hash
    );
    ensure!(
        output.deposit_queue_transition.prevDepositNumber == batch.prev_deposit_number,
        "previous deposit number mismatch: SPF {}, candidate {}",
        output.deposit_queue_transition.prevDepositNumber,
        batch.prev_deposit_number
    );
    ensure!(
        output.deposit_queue_transition.nextDepositNumber == batch.next_deposit_number,
        "next deposit number mismatch: SPF {}, candidate {}",
        output.deposit_queue_transition.nextDepositNumber,
        batch.next_deposit_number
    );
    ensure!(
        output.withdrawal_queue_hash == batch.withdrawal_queue_hash,
        "withdrawal queue hash mismatch: SPF {}, candidate {}",
        output.withdrawal_queue_hash,
        batch.withdrawal_queue_hash
    );
    ensure!(
        output.last_batch_commitment.withdrawal_batch_index == batch.withdrawal_batch_index,
        "withdrawal batch index mismatch: SPF {}, candidate {}",
        output.last_batch_commitment.withdrawal_batch_index,
        batch.withdrawal_batch_index
    );
    Ok(())
}

fn witness_size(witness: &BatchWitness) -> usize {
    witness
        .zone_state_witness
        .node_pool
        .iter()
        .chain(&witness.zone_state_witness.bytecodes)
        .chain(&witness.tempo_state_witness.node_pool)
        .chain(&witness.tempo_ancestry_headers)
        .map(|bytes| bytes.len())
        .sum::<usize>()
        + witness.tempo_state_witness.initial_tempo_header_rlp.len()
        + witness
            .zone_blocks
            .iter()
            .map(|block| {
                block.tempo_header_rlp.len()
                    + block
                        .transactions
                        .iter()
                        .map(|bytes| bytes.len())
                        .sum::<usize>()
            })
            .sum::<usize>()
}
