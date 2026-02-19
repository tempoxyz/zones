# Zone Prover & Witness Pipeline — Comprehensive Review

## Critical Issues

### 1. `zone_block_index` is always 0 in multi-block batches

**Severity**: Critical (breaks multi-block batches with L1 reads)

**Location**: `crates/tempo-zone/src/builder.rs:411`

The builder always sets `set_l1_recording_block_index(0)` because it builds one block at a time. Every `RecordedL1Read` gets `zone_block_index = 0` regardless of which block in the batch produced it.

The prover's `TempoStateAccessor` indexes reads by `(zone_block_index, account, slot)` and uses the loop index as the block index. For a 3-block batch, block 2 would look up `(2, account, slot)` but all reads have `zone_block_index = 0`, causing `TempoReadNotFound`.

**Impact**: Any multi-block batch where blocks 2+ perform L1 reads (i.e., execute `advanceTempo`) will fail with a prover error.

**Fix**: The `ProofGenerator` must re-index `zone_block_index` in the `RecordedL1Read` entries when merging across blocks. When iterating `block_witnesses`, assign the batch-relative index:

```rust
for (batch_idx, (_, bw)) in block_witnesses.iter().enumerate() {
    for mut read in bw.l1_reads.clone() {
        read.zone_block_index = batch_idx as u64;
        all_l1_reads.push(read);
    }
}
```

### 2. `tempo_ancestry_headers` contains wrong data

**Severity**: Medium (currently masked by direct mode)

**Location**: `crates/tempo-zone/src/proof.rs:121-127`

The `ProofGenerator` pushes each block's `tempo_header_rlp` into `tempo_ancestry_headers`:

```rust
tempo_ancestry_headers.push(bw.tempo_header_rlp.clone());
```

But the prover's `tempo_ancestry_headers` field is for **ancestry chain verification** — it should contain the sequence of Tempo headers from `tempo_block_number + 1` to `anchor_block_number`, used when `anchor_block_number > tempo_block_number`.

Currently this is harmless because the `ProofGenerator` always uses direct mode (`anchor_block_number == tempo_block_number`), so `tempo_ancestry_headers` is never read by the prover. But if ancestry mode is ever enabled, this will break.

**Fix**: Either:
- (a) Pass `vec![]` for `tempo_ancestry_headers` since direct mode doesn't use it, or
- (b) When ancestry mode is needed, fetch the actual ancestry chain headers from L1.

### 3. Soft proof has no authentication

**Severity**: High (expected for POC, must be addressed before production)

**Location**: `crates/tempo-zone/src/proof.rs:338-360`

The "proof" is just an ABI-packed `BatchOutput` — the sequencer's claimed result of execution. There is no cryptographic proof (ZK or TEE attestation). The L1 verifier contract must currently accept any proof bytes, meaning a malicious sequencer can submit arbitrary state transitions.

This is explicitly a POC mode placeholder, but it must be noted as the single largest security gap.

---

## High-Severity Issues

### 4. WitnessStore grows unbounded on validator nodes

**Severity**: High (resource exhaustion)

**Location**: `crates/tempo-zone/src/witness/store.rs`

On validator/full nodes (no `--sequencer.key`), the builder still records witnesses via `ZoneEvmConfig::new_with_recording()`, but no `ProofGenerator` exists to consume them. The `WitnessStore` grows with every block indefinitely. At ~1KB per block (access snapshot + zone block), this is ~86MB/day at 1 block/second.

**Fix**: Either:
- (a) Disable recording on non-sequencer nodes, or
- (b) Add a cap to `WitnessStore` that auto-prunes when exceeding a configurable limit.

### 5. `ProofGenerator` silently swallows all errors

**Severity**: High (masks bugs in production)

**Location**: `crates/tempo-zone/src/proof.rs:73-234`

Every failure path in `generate_batch_proof` logs a warning and returns empty bytes. This means:
- Missing witness data → empty proof
- StateProvider failure → empty proof
- MPT proof generation failure → empty proof
- L1 RPC failure → empty proof
- Prover logic error → empty proof

All of these produce the same externally-visible behavior: a batch submitted with no proof. There is no way to distinguish "witness data was missing for an imported block" from "there's a bug in the prover". In POC mode this is acceptable, but it masks real bugs during development.

**Fix**: Return a `Result` from `generate_batch_proof` and let the caller decide whether to fall back to empty proof. At minimum, differentiate between "expected absence" (imported block, no witness) and "unexpected failure" (prover bug).

### 6. `anchor_block_hash` assumes last block always has `tempo_header_rlp`

**Severity**: High (prover will fail if last block has no advanceTempo)

**Location**: `crates/tempo-zone/src/proof.rs:190-191`

```rust
let last_header_rlp = &block_witnesses.last().unwrap().1.tempo_header_rlp;
let anchor_block_hash = alloy_primitives::keccak256(last_header_rlp);
```

If the last block in the batch doesn't have a `tempo_header_rlp` (i.e., no `advanceTempo` was executed), this hashes an empty vec, producing `keccak256([])` which won't match the expected Tempo block hash. The prover will fail with `InconsistentState`.

**Fix**: Search backwards through block witnesses to find the last block that actually had a `tempo_header_rlp`, or read the Tempo block hash from the zone state.

---

## Medium-Severity Issues

### 7. L1 proof fetching is sequential

**Location**: `crates/tempo-zone/src/proof.rs:242-288`

Each `eth_getProof` RPC call is awaited sequentially. For batches with many distinct `(tempo_block_number, account)` groups, this adds linear latency. Using `futures::join_all` with a concurrency limiter would reduce batch proof time proportionally.

### 8. No batch size guard

**Location**: `crates/tempo-zone/src/zonemonitor.rs:206`

The monitor processes `[from, to]` where `to` can be arbitrarily far ahead of `from` (e.g., after a long outage). A 10,000-block batch would:
- Allocate a merged `AccessSnapshot` with the union of all accesses
- Generate MPT proofs for every account/slot ever touched
- Assemble a massive `BatchWitness`
- Run the prover over 10,000 blocks

This could exhaust memory or time out. A configurable max batch size (e.g., 100 blocks) with chunking would prevent this.

### 9. `BLOCKHASH` opcode returns zero

**Location**: `crates/zone-prover/src/db.rs:182-186`

The prover's `WitnessDatabase::block_hash()` always returns `B256::ZERO`. If any user contract uses the `BLOCKHASH` opcode, the prover will return different results than the node (which returns real block hashes from reth's state). This creates a **state divergence** between the node and the prover.

This is noted in a comment ("Return zero for now; if needed, the witness can include block hashes"), but it's a correctness issue if any contract on the zone uses `BLOCKHASH`.

### 10. `user_tx_bytes` extraction assumes fixed system tx positions

**Location**: `crates/tempo-zone/src/builder.rs:147-157`

```rust
let all_txs: Vec<_> = sealed_block.body().transactions().collect();
let user_tx_bytes: Vec<Vec<u8>> = if all_txs.len() >= 2 {
    all_txs[1..all_txs.len() - 1]  // skip first and last
```

This assumes `advanceTempo` is always tx[0] and `finalizeWithdrawalBatch` is always tx[last]. If the block structure ever changes (e.g., additional system txs, or blocks without `advanceTempo`), this will silently include system txs as user txs or exclude user txs.

### 11. `decryptions` is always empty

**Location**: `crates/tempo-zone/src/builder.rs:185`

```rust
decryptions: vec![],
```

The builder never populates `DecryptionData` for encrypted deposits. This is presumably because encrypted deposit support is not yet implemented, but it means any encrypted deposits on L1 will be silently dropped during zone block execution.

---

## Low-Severity / Design Notes

### 12. Validator witnesses are never pruned

Related to issue #4. The `prune_below` call only happens in the `ProofGenerator`, which doesn't exist on validator nodes. Even the `take_range` removal path doesn't execute. A background task or the builder itself should handle this.

### 13. `RecordedL1Read.value` is `B256` but `L1StateRead.value` is `U256`

**Location**: `recording_l1.rs:25` vs `types.rs L1StateRead`

The recorded read stores the value as `B256` (raw 32 bytes from the storage reader), while the prover's `L1StateRead` uses `U256`. The conversion in `generate_tempo_state_proof` is `U256::from_be_bytes(r.value.0)`, which is correct but the type mismatch is a source of potential confusion. Consider aligning the types.

### 14. `finalize_withdrawal_batch_count` is always `U256::MAX`

**Location**: `crates/tempo-zone/src/builder.rs:186`

The builder passes `U256::MAX` as the count to `finalizeWithdrawalBatch`, meaning "finalize all pending". This is correct for the current single-sequencer model but worth noting — there's no support for partial batch finalization.

### 15. `prev_block_header` in `BuiltBlockWitness` is the parent's parent header

**Location**: `crates/tempo-zone/src/builder.rs:192-200`

The `prev_block_header` stored in `BuiltBlockWitness` is built from `parent_header` (the parent of the block being built). For the first block in a batch, this is the correct "prev_block_header" for the prover. But for subsequent blocks in a multi-block batch, the `ProofGenerator` only uses the first block's `prev_block_header`, which is correct. Just worth noting that `prev_block_header` in later blocks is unused.

---

## Architectural Assessment

### Good Decisions

1. **Separation of prover from node**: `zone-prover` is a pure crate with no I/O, async, or node dependencies. This enables ZK/TEE portability.

2. **WitnessDatabase hard-fails on missing data**: This is the correct security posture — the prover cannot silently invent zero state.

3. **Deduplicated node pool**: Sharing MPT proof nodes across all Tempo reads is an excellent optimization for proof size and verification cost.

4. **ProofGenerator using direct StateProvider**: Eliminating the RPC round-trip for zone state proofs is a major efficiency win.

5. **Access recording architecture**: Recording raw accesses in the builder and deferring proof generation to batch time is the right design for multi-block batches.

### Questionable Decisions

1. **Single-block batch assumption baked into builder**: The builder hardcodes `zone_block_index = 0` and extracts user txs by position. Multi-block batching was retrofitted rather than designed in, creating subtle bugs (#1, #10).

2. **Empty proof fallback without distinction**: Every proof failure produces the same empty bytes. Consider at minimum an enum: `ProofResult::Valid(bytes)`, `ProofResult::Unavailable`, `ProofResult::Failed(error)`.

3. **Witness recording on validators**: Recording is always enabled even when no consumer exists. This was probably done for simplicity (single `ZoneEvmConfig` path) but has a real resource cost (#4).
