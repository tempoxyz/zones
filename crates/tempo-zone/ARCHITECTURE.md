# Tempo Zone Node Architecture

## Overview

The zone node is a lightweight L2 built on reth that executes zone blocks driven by Tempo L1 events. It bridges deposits from L1, executes user transactions, and submits batch proofs back to L1.

## Data Flow

```
                            Tempo L1
                   ┌─────────────────────────┐
                   │       ZonePortal         │
                   │  deposit() submitBatch() │
                   │  processWithdrawal()     │
                   └──┬──────────┬────────┬───┘
                      │          │        │
              WS subscribe   submitBatch  processWithdrawal
              + getLogs      (sequencer)  (sequencer)
                      │          ▲        ▲
                      ▼          │        │
               ┌──────────┐     │   ┌────────────────────┐
               │L1Subscriber│    │   │WithdrawalProcessor │
               │(critical) │    │   │(sequencer only)    │
               └─────┬─────┘    │   └──────────▲─────────┘
                     │          │              │ notify
                     ▼          │              │
              ┌────────────┐    │    ┌─────────┴──────────┐
              │DepositQueue│    │    │  SharedWithdrawal   │
              │(Arc<Mutex>)│    │    │  Store              │
              └─────┬──────┘    │    └──────────▲──────────┘
                    │ notify    │               │
                    ▼           │               │
              ┌──────────┐     │     ┌─────────┴──────────┐
              │ZoneEngine│     │     │   ZoneMonitor       │
              │(critical)│     │     │  - poll Zone L2     │
              └────┬─────┘     │     │  - collect events   │
                   │ FCU       │     │  - build BatchData  │
                   ▼           │     └──────────▲──────────┘
         ┌──────────────────┐  │                │
         │ZonePayloadBuilder│  │     ┌──────────┴──────────┐
         │ - advanceTempo   │  │     │  ProofGenerator      │
         │ - pool txs       │  │     │  - merge accesses    │
         │ - finalizeWdBatch│  │     │  - local MPT proofs  │
         │ - RecordingDB    │  │     │  - L1 eth_getProof   │
         └───────┬──────────┘  │     │  - assemble witness  │
                 │             │     │  - prove_zone_batch() │
                 ▼             │     └──────────▲──────────┘
         ┌──────────────────┐  │                │
         │SharedWitnessStore│──┼────────────────┘
         │(Arc<Mutex>)      │  │     take_range + prune
         └──────────────────┘  │
                               │
                     ┌─────────┴─────────┐
                     │  BatchSubmitter    │
                     │  ZonePortal.      │
                     │  submitBatch()    │
                     └───────────────────┘
```

## Task Boundaries

| Task | Type | Description |
|---|---|---|
| `l1-deposit-subscriber` | `spawn_critical` | WS subscription to L1, populates DepositQueue |
| `zone-engine` | `spawn_critical` | Drives block production via FCU |
| `l1-state-listener` | `spawn_critical` | Caches L1 state for TempoStateReader precompile |
| zone monitor | `tokio::spawn` | Polls L2, generates proofs, submits batches (sequencer only) |
| withdrawal processor | `tokio::spawn` | Processes withdrawals on L1 (sequencer only) |

## Witness Generation Pipeline

The pipeline bridges the builder (which executes blocks) and the prover (which re-executes from proofs).

### Phase 1: Recording (builder, per block)

```
ZonePayloadBuilder::try_build()
  └─ wraps DB in RecordingDatabase
  └─ wraps L1 provider in RecordingL1StateProvider
  └─ executes advanceTempo + pool txs + finalizeWithdrawalBatch
  └─ snapshots RecordedAccesses → AccessSnapshot { accounts, storage }
  └─ snapshots RecordedL1Reads → Vec<RecordedL1Read>
  └─ stores BuiltBlockWitness in SharedWitnessStore
```

### Phase 2: Proof Generation (ProofGenerator, per batch)

```
ProofGenerator::generate_batch_proof(from, to)
  └─ prune_below(from) + take_range(from, to) from WitnessStore
  └─ merge all AccessSnapshots into union
  └─ state_by_block_hash(S₀) → StateProvider
  └─ generate_zone_state_witness() — local trie walk, MPT proofs against S₀
  └─ fetch_l1_proofs() — eth_getProof over RPC for Tempo L1 reads
  └─ generate_tempo_state_proof() — deduplicate MPT nodes into pool
  └─ assemble_witness() → BatchWitness
  └─ prove_zone_batch() → BatchOutput
  └─ encode_batch_output() → 192-byte soft proof
```

### Why Proofs Are Generated Against S₀

The prover bootstraps a single `WitnessDatabase` from the initial zone state (S₀ — the state before any block in the batch). All blocks in the batch execute against this database. Therefore, all MPT proofs must be valid against S₀'s state root, not intermediate state roots. The builder records raw access sets (not proofs), and the `ProofGenerator` merges them into a union before generating proofs against S₀.

## Sequencer vs Validator

- **Sequencer** (`--sequencer.key` present): All components active — builder records witnesses, ProofGenerator proves, monitor submits batches, withdrawal processor runs.
- **Validator** (no key): Core node only — builder still records (always-on via `ZoneEvmConfig::new_with_recording`), but no monitor/prover/submitter. WitnessStore accumulates but is never consumed.

## Current Limitations (POC)

1. **Single-block-per-batch**: The builder always produces exactly one block per batch. Every block includes both `advanceTempo` and `finalizeWithdrawalBatch` system transactions, and `zone_block_index` is hardcoded to 0. Multi-block batching would require: re-indexing `zone_block_index` across blocks, making `finalizeWithdrawalBatch` conditional on final-block status, and fixing user tx extraction to handle varying system tx positions.

2. **Soft proof (no authentication)**: The proof is ABI-packed `BatchOutput` with no ZK or TEE attestation. The L1 verifier must accept any proof bytes.

3. **Unbounded WitnessStore on validators**: Recording is always-on. On non-sequencer nodes the store grows indefinitely since no `ProofGenerator` consumes entries.

4. **Sequential L1 proof fetching**: `eth_getProof` calls in `ProofGenerator` are sequential; could be parallelized.

5. **No batch size cap**: The monitor processes arbitrarily large block ranges, risking memory pressure on catch-up after outages.

## Error Handling

- **ProofGenerator**: Returns `(empty, empty)` on any failure — batch still submitted with empty proof (POC mode).
- **BatchSubmitter**: Retries 3x with exponential backoff (2s/4s/8s). On exhaustion, resyncs `prev_block_hash` from portal.
- **L1Subscriber/L1StateListener**: Auto-reconnect after 5s on failure.
- **ZoneEngine**: 100ms pause on build failure, retries on next loop iteration.
