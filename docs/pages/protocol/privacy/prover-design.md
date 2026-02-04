# Zone Prover Design

## State Transition Function

The zone prover implements a pure state transition function in Rust with `no_std` compatibility, allowing it to run in both ZKVMs like SP1 and TEEs like SGX or TDX.

The function takes a complete witness of zone blocks and their dependencies, executes the EVM state transitions (including required system transactions), and outputs commitments for on-chain verification.
The core commitment is the **zone block hash transition** (not the raw state root), matching the privacy zone spec and Solidity reference implementation.

## Interface

```rust
#![no_std]

/// Pure state transition function for zone batch proving
pub fn prove_zone_batch(witness: BatchWitness) -> Result<BatchOutput, Error>
```

## Witness Structure

```rust
pub struct PublicInputs {
    /// Previous batch's block hash (must equal portal.blockHash)
    pub prev_block_hash: B256,

    /// Tempo block number for the batch (must equal portal's tempoBlockNumber)
    pub tempo_block_number: u64,

    /// Tempo block hash for the batch (must equal portal's EIP-2935 lookup)
    pub tempo_block_hash: B256,

    /// Expected withdrawal batch index (passed by portal as withdrawalBatchIndex + 1)
    pub expected_withdrawal_batch_index: u64,

    /// Registered sequencer (passed by portal; zone block beneficiary must match)
    pub sequencer: Address,
}

pub struct BatchWitness {
    /// Public inputs committed by the proof system
    pub public_inputs: PublicInputs,

    /// Previous batch's block header (for state-root binding)
    pub prev_block_header: ZoneHeader,

    /// Zone blocks to execute
    pub zone_blocks: Vec<ZoneBlock>,

    /// Initial zone state
    pub initial_zone_state: ZoneStateWitness,

    /// Tempo state proofs for Tempo reads
    pub tempo_state_proofs: BatchStateProof,
}

pub struct BatchOutput {
    /// Zone block hash transition (prev -> final)
    pub block_transition: BlockTransition,

    /// Deposit queue processing
    pub deposit_queue_transition: DepositQueueTransition,

    /// Withdrawal queue updates
    pub withdrawal_queue_transition: WithdrawalQueueTransition,

    /// Withdrawal batch parameters read from ZoneOutbox.lastBatch
    pub last_batch: LastBatchCommitment,
}

pub struct LastBatchCommitment {
    pub withdrawal_batch_index: u64,
}

pub struct ZoneHeader {
    pub parent_hash: B256,
    pub beneficiary: Address,
    pub state_root: B256,
    pub transactions_root: B256,
    pub receipts_root: B256,
    pub number: u64,
    pub timestamp: u64,
}

#[derive(Debug)]
pub enum Error {
    InvalidProof,
    ExecutionError(String),
    InconsistentState,
}
```

## Components

### Zone Blocks

```rust
pub struct ZoneBlock {
    /// Block number
    pub number: u64,

    /// Parent block hash
    pub parent_hash: B256,

    /// Timestamp
    pub timestamp: u64,

    /// Beneficiary (must match registered sequencer)
    pub beneficiary: Address,

    /// Tempo header RLP used by the system tx (ZoneInbox.advanceTempo)
    pub tempo_header_rlp: Vec<u8>,

    /// Deposits processed by the system tx (oldest first, unified queue)
    pub deposits: Vec<QueuedDeposit>,

    /// Decryption data for encrypted deposits in the system tx
    pub decryptions: Vec<DecryptionData>,

    /// System tx at end of block (ZoneOutbox.finalizeWithdrawalBatch)
    /// Required for the final block in a batch; must be absent in intermediate blocks.
    pub finalize_withdrawal_batch_count: Option<u64>,

    /// Transactions to execute
    pub transactions: Vec<Transaction>,
}
```

Each zone block contains the required system transactions plus user transactions that will be executed using `revm`.
The system transactions must mirror the Solidity reference implementation:
- `ZoneInbox.advanceTempo(tempo_header_rlp, deposits, decryptions)` at the start of the block
- `ZoneOutbox.finalizeWithdrawalBatch(count)` at the end of the block **only if this is the final block of the batch**

User transactions may call the `TempoState` precompile to read Tempo state. User transactions
**must not** call `ZoneInbox` or `ZoneOutbox`; those are system-only predeploys. The executor
must reject any non-system call to these addresses and enforce exactly one `advanceTempo`
at the start of each block and exactly one `finalizeWithdrawalBatch` in the final block of the batch.
The block hash is computed from the simplified zone header:
`parentHash`, `beneficiary`, `stateRoot`, `transactionsRoot`, `receiptsRoot`, `number`, `timestamp`.
The transactions and receipts roots are computed over the full ordered list:
`[advanceTempo, user txs..., finalizeWithdrawalBatch?]`.

### Zone State Witness

```rust
pub struct ZoneStateWitness {
    /// Account data with storage proofs
    pub accounts: HashMap<Address, AccountWitness>,

    /// Zone state root at start of batch
    pub state_root: B256,
}

pub struct AccountWitness {
    pub nonce: u64,
    pub balance: U256,
    pub code_hash: B256,
    pub code: Option<Vec<u8>>,

    /// Storage slots with values
    pub storage: HashMap<U256, U256>,

    /// MPT proof for account
    pub account_proof: Vec<Vec<u8>>,

    /// MPT proofs for storage slots
    pub storage_proofs: HashMap<U256, Vec<Vec<u8>>>,
}
```

The witness only includes accounts and storage slots that will be accessed during batch execution. Standard MPT proofs allow verification against the zone state root. Any
account or storage access not present in the witness must be treated as an error (do not
default to zero) so the prover cannot omit non-zero state.

### Tempo State Proofs

```rust
pub struct BatchStateProof {
    /// Deduplicated pool of all MPT nodes
    pub node_pool: HashMap<B256, Vec<u8>>,

    /// Tempo state reads with proof paths
    pub reads: Vec<L1StateRead>,
}

pub struct L1StateRead {
    /// Which zone block performed this read
    pub zone_block_index: u32,

    /// Which Tempo block to read from (must match TempoState for this block)
    pub tempo_block_number: u64,

    /// Tempo account and storage slot
    pub account: Address,
    pub slot: U256,

    /// Path through node_pool from leaf to state root
    pub node_path: Vec<B256>,

    /// Expected value
    pub value: U256,
}
```

The `BatchStateProof` structure enables efficient proving of potentially thousands of Tempo state reads across multiple zone blocks.

**Binding to Tempo:**
- Tempo headers are validated by executing `TempoState.finalizeTempo` during the system tx
  in each zone block. The resulting `tempoBlockNumber`, `tempoBlockHash`, and
  `tempoStateRoot` are part of the proven zone state.
- `TempoState.tempoBlockNumber()` at end of batch must equal `public_inputs.tempo_block_number`.
- Each Tempo read is verified against the `tempoStateRoot` currently bound in `TempoState`
  at the time of the read. The precompile must reject reads if the block is not yet bound.
- For a given zone block, all Tempo reads must use the Tempo block number finalized by that
  block's `advanceTempo` system tx (checked against `TempoState.tempoBlockNumber()`).
- Tempo reads performed inside `advanceTempo` (e.g., deposit queue hash) must be bound to the
  Tempo header finalized by that call.

Inside execution, `TempoState.readTempoStorageSlot` is modeled to read from the current `tempoStateRoot` (derived from the finalized header), so the proof and the precompile agree on the same root.

#### Deduplication Strategy

The key optimization is the **deduplicated node pool**. Instead of including separate MPT proofs for each Tempo storage read, all proofs share a single pool of verified nodes.

**Why this matters:**
- A batch might perform 100,000 Tempo state reads across 100 zone blocks
- Many reads access the same accounts (shared account trie paths)
- Many reads access slots in nearby addresses (shared storage trie paths)
- Across multiple Tempo blocks, unchanged state shares identical nodes

**How it works:**

1. `node_pool` contains every unique MPT node, keyed by `keccak256(rlp(node))`
2. Each `L1StateRead` has a `node_path` that references nodes in the pool by hash
3. During proving:
   - Verify each node in the pool exactly once: `keccak256(node) == hash`
   - For each read, walk the `node_path` through verified nodes
   - No node is ever hashed more than once

**Example:**

```
Zone block 0: Read Account A slot 5 from Tempo block 1000
Zone block 1: Read Account A slot 6 from Tempo block 1000
Zone block 2: Read Account A slot 5 from Tempo block 1001

Traditional approach:
  - 3 separate proofs × 8 nodes each = 24 node verifications
  - Many nodes overlap but are verified multiple times

Deduplicated approach:
  node_pool = {
    0xaaa... -> [branch node],      // shared by all 3 reads
    0xbbb... -> [branch node],      // shared by all 3 reads
    0xccc... -> [extension node],   // shared by all 3 reads
    0xddd... -> [leaf for A[5] in block 1000],
    0xeee... -> [leaf for A[6] in block 1000],
    0xfff... -> [leaf for A[5] in block 1001],
  }
  Total: ~11 unique nodes verified once each
```

**Proof size reduction:**
- Traditional: 100,000 reads × 8 nodes × 32 bytes = 25.6 MB
- Deduplicated: ~50,000 unique nodes × 32 bytes = 1.6 MB
- **Compression: ~16x smaller**

**Prover cost reduction:**
- Traditional: 800,000 keccak operations
- Deduplicated: 50,000 keccak operations
- **Speedup: ~16x faster**

## Implementation

```rust
pub fn prove_zone_batch(witness: BatchWitness) -> Result<BatchOutput, Error> {
    // Phase 1: Verify Tempo state proofs
    let tempo_state = verify_tempo_proofs(&witness.tempo_state_proofs)?;

    // Phase 2: Initialize zone state
    let mut zone_state = ZoneState::from_witness(&witness.initial_zone_state)?;

    // Bind initial state root to the previous block hash
    if zone_state.state_root() != witness.prev_block_header.state_root {
        return Err(Error::InconsistentState);
    }
    if keccak256(rlp_encode(&witness.prev_block_header)) != witness.public_inputs.prev_block_hash {
        return Err(Error::InvalidProof);
    }

    // Capture deposit queue start
    let deposit_prev = zone_state.zone_inbox_processed_hash()?;

    // Phase 3: Execute zone blocks and compute block hashes
    let mut prev_block_hash = witness.public_inputs.prev_block_hash;
    let mut prev_header = witness.prev_block_header;

    for (idx, block) in witness.zone_blocks.iter().enumerate() {
        let is_last_block = idx + 1 == witness.zone_blocks.len();

        if block.parent_hash != prev_block_hash {
            return Err(Error::InconsistentState);
        }
        if block.number != prev_header.number + 1 {
            return Err(Error::InconsistentState);
        }
        if block.timestamp < prev_header.timestamp {
            return Err(Error::InconsistentState);
        }
        if block.beneficiary != witness.public_inputs.sequencer {
            return Err(Error::InconsistentState);
        }

        if is_last_block && block.finalize_withdrawal_batch_count.is_none() {
            return Err(Error::InconsistentState);
        }
        if !is_last_block && block.finalize_withdrawal_batch_count.is_some() {
            return Err(Error::InconsistentState);
        }

        // Execute block with system txs + user txs, and Tempo access via tempo_state
        let (tx_root, receipts_root, finalized_tempo_number) =
            execute_zone_block(&mut zone_state, block, &tempo_state, idx)?;

        // Build the zone block header and compute the block hash
        let header = ZoneHeader {
            parent_hash: prev_block_hash,
            beneficiary: block.beneficiary,
            state_root: zone_state.state_root(),
            transactions_root: tx_root,
            receipts_root: receipts_root,
            number: block.number,
            timestamp: block.timestamp,
        };
        prev_block_hash = keccak256(rlp_encode(header));
        prev_header = header;

        // Bind the block's Tempo header to the finalized Tempo state number
        let expected_tempo_hash = tempo_state
            .block_hash(finalized_tempo_number)
            .ok_or(Error::InvalidProof)?;
        if expected_tempo_hash != keccak256(&block.tempo_header_rlp) {
            return Err(Error::InconsistentState);
        }
    }

    // Phase 4: Extract output commitments
    let deposit_next = zone_state.zone_inbox_processed_hash()?;
    let last_batch = zone_state.zone_outbox_last_batch()?;
    let tempo_number = zone_state.tempo_state_block_number()?;

    // Ensure TempoState is bound to public inputs for this batch
    let tempo_hash = tempo_state
        .block_hash(tempo_number)
        .ok_or(Error::InvalidProof)?;
    if tempo_hash != witness.public_inputs.tempo_block_hash {
        return Err(Error::InconsistentState);
    }
    if tempo_number != witness.public_inputs.tempo_block_number {
        return Err(Error::InconsistentState);
    }

    Ok(BatchOutput {
        block_transition: BlockTransition {
            prev_block_hash: witness.public_inputs.prev_block_hash,
            next_block_hash: prev_block_hash,
        },
        deposit_queue_transition: DepositQueueTransition {
            prev_processed_hash: deposit_prev,
            next_processed_hash: deposit_next,
        },
        withdrawal_queue_transition: WithdrawalQueueTransition {
            withdrawal_queue_hash: last_batch.withdrawal_queue_hash,
        },
        last_batch: LastBatchCommitment {
            withdrawal_batch_index: last_batch.withdrawal_batch_index,
        },
    })
}

fn verify_tempo_proofs(
    proofs: &BatchStateProof,
) -> Result<TempoStateAccessor, Error> {
    // Verify each node in pool exactly once
    let mut verified_nodes = HashMap::new();
    for (claimed_hash, rlp_data) in &proofs.node_pool {
        let actual_hash = keccak256(rlp_data);
        if actual_hash != *claimed_hash {
            return Err(Error::InvalidProof);
        }
        verified_nodes.insert(*claimed_hash, MptNode::decode(rlp_data)?);
    }

    // Index read proofs for on-demand verification during execution
    let mut read_index = HashMap::new();
    for read in &proofs.reads {
        read_index.insert(
            (read.zone_block_index, read.account, read.slot),
            read,
        );
    }

    Ok(TempoStateAccessor { verified_nodes, read_index })
}

fn execute_zone_block(
    zone_state: &mut ZoneState,
    block: &ZoneBlock,
    tempo_state: &TempoStateAccessor,
    block_index: usize,
) -> Result<(B256, B256, u64), Error> {
    // Set up revm with TempoState precompile
    let mut evm = revm::EVM::builder()
        .with_db(zone_state)
        .with_block_env(block_env_from(block))
        .with_precompile(
            TEMPO_STATE_ADDRESS,
            TempoStatePrecompile::new(tempo_state, block_index),
        )
        .build();

    // System tx: advance Tempo and process deposits.
    // The TempoState precompile must bind reads during this call to the newly finalized
    // Tempo header, and reject any unbound reads.
    evm.transact_commit(system_tx_advance_tempo(
        &block.tempo_header_rlp,
        &block.deposits,
        &block.decryptions,
    ))
    .map_err(|e| Error::ExecutionError(e.to_string()))?;

    let finalized_tempo_number = zone_state.tempo_state_block_number()?;
    tempo_state.bind_block(block_index, finalized_tempo_number)?;

    // Execute transactions
    for tx in &block.transactions {
        evm.transact_commit(tx)
            .map_err(|e| Error::ExecutionError(e.to_string()))?;
    }

    // Optional system tx: finalize batch
    let finalize_tx = block
        .finalize_withdrawal_batch_count
        .map(system_tx_finalize_withdrawal_batch);
    if let Some(tx) = &finalize_tx {
        evm.transact_commit(tx)
            .map_err(|e| Error::ExecutionError(e.to_string()))?;
    }

    // Compute roots for block hash commitment
    let tx_root = compute_transactions_root(
        &system_tx_advance_tempo(&block.tempo_header_rlp, &block.deposits, &block.decryptions),
        &block.transactions,
        finalize_tx.as_ref(),
    );
    let receipts_root = compute_receipts_root(evm.last_block_receipts());

    Ok((tx_root, receipts_root, finalized_tempo_number))
}
```

## Deployment Modes

### ZKVM (SP1)

```rust
#[cfg(target_os = "zkvm")]
fn main() {
    let witness: BatchWitness = zkvm::io::read();
    let output = prove_zone_batch(witness).expect("proof generation failed");
    zkvm::io::commit(&output);
}
```

### TEE (SGX/TDX)

```rust
#[cfg(target_env = "sgx")]
#[no_mangle]
pub extern "C" fn ecall_prove_batch(
    witness_ptr: *const u8,
    witness_len: usize,
) -> BatchOutput {
    let witness = unsafe { deserialize(witness_ptr, witness_len) };
    prove_zone_batch(witness).expect("proof generation failed")
}
```

## On-Chain Verification

The portal contract receives:
- `tempoBlockNumber` - validates against EIP-2935 block hash history and passes to the verifier
- `blockTransition` - from `BatchOutput` (block hash based)
- `proof` - ZKVM proof or TEE attestation

The portal passes the following to the verifier:
- `tempoBlockNumber` and `tempoBlockHash` (from EIP-2935)
- `expectedWithdrawalBatchIndex` (portal's `withdrawalBatchIndex + 1`)
- `sequencer` (the registered sequencer address)
- `blockTransition`, `depositQueueTransition`, `withdrawalQueueTransition`
- `verifierConfig` and `proof`

The verifier validates that the prover correctly executed the state transition and produced the output commitments.
In particular, the proof must enforce:
- `TempoState.tempoBlockHash == tempoBlockHash` from the portal (EIP-2935) for `tempoBlockNumber`
- `TempoState.tempoBlockNumber == tempoBlockNumber`
- `ZoneOutbox.lastBatch().withdrawalBatchIndex == expectedWithdrawalBatchIndex` (passed by portal)
- `ZoneOutbox.lastBatch().withdrawalQueueHash == withdrawalQueueTransition.withdrawalQueueHash`
- Zone block `beneficiary` equals `sequencer` (passed by portal)
- `DepositQueueTransition` matches `ZoneInbox.processedDepositQueueHash` changes
- `BlockTransition` is computed from the zone block header hash (not raw state root)
