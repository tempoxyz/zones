# Zone Prover Architecture

## Overview

The zone prover implements a **pure state transition function** that re-executes zone blocks from a cryptographic witness rather than a full state database. It takes a `BatchWitness` (initial state proofs, blocks, Tempo L1 proofs) and produces a `BatchOutput` (block hash transitions, deposit/withdrawal commitments).

The function is designed for execution inside ZKVMs (SP1) or TEEs (SGX/TDX) — it has no I/O, no async, and no access to the node's state.

## Entry Point

```
prove_zone_batch(witness: BatchWitness) -> Result<BatchOutput, ProverError>
```

## Execution Phases

```
┌───────────────────────────────────────────────────────────────────┐
│ Phase 1: Verify Tempo State Proofs                                │
│                                                                   │
│  BatchStateProof.node_pool  ──verify keccak256──▶ TempoStateAccessor
│  BatchStateProof.reads      ──index by (block, account, slot)──┘  │
│  BatchStateProof.account_proofs ──index by (block, account)──┘    │
└───────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌───────────────────────────────────────────────────────────────────┐
│ Phase 2: Initialize Zone State                                    │
│                                                                   │
│  ZoneStateWitness ──verify MPT proofs──▶ WitnessDatabase (revm DB)│
│  + bind state root to prev_block_header                           │
│  + bind prev_block_header hash to public_inputs.prev_block_hash   │
│  + read initial deposit hash, Tempo state, predeploy values       │
│  + build sparse tries for state root recomputation                │
└───────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌───────────────────────────────────────────────────────────────────┐
│ Phase 3: Execute Zone Blocks (loop)                               │
│                                                                   │
│  For each ZoneBlock:                                              │
│    1. Validate linkage (parent hash, number, timestamp, sequencer)│
│    2. Update Tempo binding if tempo_header_rlp present            │
│    3. Execute via TempoEvm:                                       │
│       a. advanceTempo system tx (if present)                      │
│       b. User transactions (RLP-decoded, signature-recovered)     │
│       c. finalizeWithdrawalBatch system tx (final block only)     │
│    4. Extract BundleState, compute new state root via sparse trie │
│    5. Assert computed root == block.expected_state_root            │
│    6. Build ZoneHeader, compute block hash                        │
└───────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌───────────────────────────────────────────────────────────────────┐
│ Phase 4: Extract Output Commitments                               │
│                                                                   │
│  Read final deposit queue hash, LastBatch from zone state         │
│  Validate TempoState.tempoBlockNumber == public_inputs            │
│  Validate anchor binding (direct or ancestry mode)                │
│  Return BatchOutput                                               │
└───────────────────────────────────────────────────────────────────┘
```

## Key Components

### WitnessDatabase (`db.rs`)

Implements revm's `Database` trait. Constructed from `ZoneStateWitness` by verifying every account and storage MPT proof against the state root. **Critical invariant**: any access to an account or slot not present in the witness is a hard `MissingWitness` error — the prover never silently returns zero for unwitnessed state.

#### Block Hash Resolution (EIP-2935)

The `BLOCKHASH` opcode is served via the EIP-2935 history storage contract rather than a separate witness field. The contract at `HISTORY_STORAGE_ADDRESS` stores block hashes in a ring buffer (slot = `block_number % 8191`). The prover's flow:

1. **Pre-execution**: The prover applies the EIP-2935 system call each block, writing `parent_hash` to the contract's storage (matching the node's `apply_pre_execution_changes()`).
2. **Intra-batch cache**: Before each block, the parent hash is inserted into `State.block_hashes` so that `BLOCKHASH(N-1)` hits the cache instead of the underlying WitnessDatabase (the journal write from the system call isn't visible to `Database::block_hash()`).
3. **Historical lookups**: For blocks before the batch, `WitnessDatabase::block_hash(n)` reads from the 2935 contract's storage in the initial witness.
4. **Recording**: The node's `RecordingDatabase::block_hash(n)` additionally records a storage access to `(HISTORY_STORAGE_ADDRESS, n % 8191)`, ensuring the slot enters the access snapshot and witness proofs.

### TempoStateAccessor (`tempo.rs`)

Replaces the node's RPC-based `TempoStateReader` precompile with a proof-backed accessor. Each L1 storage read triggers a full MPT verification chain: account proof against the L1 state root, then storage proof against the account's storage root. All proof nodes are shared through a deduplicated pool (`node_pool`) to avoid redundant hashing.

### SparseTrie (`sparse_mpt.rs`)

Enables stateless state root computation. Built from MPT proofs, it records known leaves and blinded branches (hash-only nodes for subtrees not touched by execution). After EVM execution, changed leaves are updated and the trie root is recomputed via `HashBuilder`.

### Predeploy Addresses

| Address | Contract | Role |
|---|---|---|
| `0x1c00..0000` | TempoState | Stores Tempo L1 block number, hash, state root |
| `0x1c00..0001` | ZoneInbox | Processes deposits, tracks deposit queue hash |
| `0x1c00..0002` | ZoneOutbox | Manages withdrawal batches |
| `0x1c00..0004` | TempoStateReader | Precompile for L1 state reads |

### Error Model

All errors are explicit and descriptive:
- `InvalidProof` — MPT verification failure
- `ExecutionError` — EVM execution failure
- `InconsistentState` — State linkage or binding mismatch
- `MissingWitness` — Unwitnessed account/slot/code access
- `TempoReadNotFound` — L1 read not in proof set
