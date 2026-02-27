# Tempo Zone Node

A lightweight L2 node built on [reth](https://github.com/paradigmxyz/reth) that
derives its state from Tempo L1.

## Overview

A **zone** is a Tempo L2 that processes one L1 block per zone block. The
sequencer watches the L1 chain for deposit, withdrawal, and token-enablement
events, builds zone blocks that execute those events via a system transaction,
and periodically submits batch proofs back to the L1 portal contract.

## Architecture

```text
                    Tempo L1
                       │
        ┌──────────────┼──────────────┐
        │              │              │
   L1Subscriber   L1StateListener  WithdrawalProcessor
        │              │              │
        ▼              ▼              │
  DepositQueue    L1StateCache        │
        │              │              │
        ▼              ▼              │
   ZoneEngine ──► ZoneEvmConfig       │
        │              │              │
        ▼              ▼              │
  PayloadBuilder  ZonePrecompiles     │
        │                             │
        ▼                             │
   ZoneMonitor ──► BatchSubmitter ────┘
                       │
                       ▼
                  ZonePortal (L1)
```

## Block Production

Each zone block processes exactly one L1 block. The flow is driven by the
[`ZoneEngine`](src/engine.rs):

1. **L1Subscriber** connects to L1 via WebSocket, backfills missed blocks, and
   enqueues `L1BlockDeposits` into the `DepositQueue`.
2. **ZoneEngine** peeks the next L1 block from the queue and builds
   `ZonePayloadAttributes` containing the L1 header, deposits, and enabled
   tokens.
3. The **payload builder** constructs an `advanceTempo` system transaction that
   calls `ZoneInbox.advanceTempo(header, deposits, decryptions, enabledTokens)`.
   This is always the first transaction in a zone block.
4. Pool transactions are appended after the system transaction, followed by a
   withdrawal batch finalization if applicable.
5. After `newPayload` succeeds, the engine **confirms** the L1 block in the
   deposit queue (removing it). On failure the block stays for retry.

The zone uses **instant finality** — head, safe, and finalized all point to the
same block.

## State Derivation

Zone state is fully derived from L1 events. The `advanceTempo` system
transaction atomically:

- Advances `tempoBlockNumber` and `tempoBlockHash` in the `TempoState`
  predeploy, anchoring the zone to L1.
- Enables newly bridged TIP-20 tokens via the `ZoneTokenFactory` precompile.
- Processes deposits from the L1 queue — minting zone-side tokens to recipients.
- Validates the deposit hash chain against the L1 portal's queue hash.

Chain continuity is enforced: the L1 block number must equal
`tempoBlockNumber + 1` and its parent hash must match the stored
`tempoBlockHash`.

### Encrypted Deposits

Deposits can be encrypted using ECIES with the sequencer's public key. The
sequencer decrypts them off-chain and provides `DecryptionData` (ECDH shared
secret + Chaum-Pedersen proof) that the contract verifies on-chain via two
precompiles before minting.

## Token Enablement

TIP-20 tokens are enabled on the zone at runtime (not at genesis). When a new
token is bridged via the L1 portal's `enableToken()`, the `L1Subscriber` picks
up the `TokenEnabled` event and includes it in the next zone block's system
transaction. The `ZoneInbox` contract calls the `ZoneTokenFactory` precompile,
which initializes the token's storage and grants `ISSUER_ROLE` to the inbox
(for minting on deposits) and outbox (for burning on withdrawals).

## Batch Submission

The [`ZoneMonitor`](src/zonemonitor.rs) watches the zone chain for new blocks,
aggregates multiple zone blocks into a single batch, and submits it to the
`ZonePortal` on L1 via [`BatchSubmitter`](src/batch.rs). Each batch contains:

- A block state transition (previous → new block hash)
- A deposit queue transition (proving which deposits were processed)
- A withdrawal hash chain (so L1 can process withdrawals back to users)

The portal verifies `tempoBlockNumber` via EIP-2935 (last 8192 block hashes).
If the zone falls behind, the submitter switches to **ancestry mode** with a
header chain linking back to the target block.

## Withdrawals

1. Users call `ZoneOutbox.requestWithdrawal()` on the zone.
2. The zone monitor collects `WithdrawalRequested` events and stores them in the
   `WithdrawalStore`.
3. At batch finalization, withdrawals are hashed into a chain and submitted to
   L1 as part of the batch proof.
4. The `WithdrawalProcessor` polls the L1 portal queue and calls
   `processWithdrawal` for each pending withdrawal.

## Zone Precompiles

| Address | Precompile | Purpose |
|---------|-----------|---------|
| `0x1C00…0004` | `TempoStateReader` | Read L1 contract storage from zone contracts |
| `0x1C00…0100` | `ChaumPedersenVerify` | Verify DLOG equality proofs for ECDH |
| `0x1C00…0101` | `AesGcmDecrypt` | AES-256-GCM authenticated decryption |
| `0x20FC…0000` | `ZoneTokenFactory` | Initialize TIP-20 tokens on the zone |

## EVM Configuration

[`ZoneEvmConfig`](src/evm.rs) wraps `TempoEvmConfig` with a `ZoneEvmFactory`
that registers the zone-specific precompiles. The
[`ZoneBlockExecutor`](src/executor.rs) is a simplified executor that does
**not** enforce subblock ordering, shared-gas accounting, or end-of-block
metadata — those are L1-only concerns.

## Module Map

| Module | Description |
|--------|-------------|
| [`abi`](src/abi.rs) | Solidity ABI bindings for ZonePortal, ZoneInbox, ZoneOutbox |
| [`batch`](src/batch.rs) | L1 batch submission and EIP-2935 ancestry logic |
| [`builder`](src/builder.rs) | Payload builder — `advanceTempo` + pool txs |
| [`engine`](src/engine.rs) | L1-event-driven block production loop |
| [`evm`](src/evm.rs) | Zone EVM factory and configuration |
| [`executor`](src/executor.rs) | Simplified block executor (no subblocks) |
| [`l1`](src/l1.rs) | L1 WebSocket subscriber and deposit queue |
| [`l1_state`](src/l1_state) | L1 storage cache, listener, and reader precompile |
| [`node`](src/node.rs) | reth node builder configuration |
| [`payload`](src/payload.rs) | Zone-specific payload attribute types |
| [`precompiles`](src/precompiles) | Chaum-Pedersen, AES-GCM, TIP20 factory, ECIES |
| [`rpc`](src/rpc) | Authenticated JSON-RPC with per-caller privacy |
| [`withdrawals`](src/withdrawals.rs) | Withdrawal store and L1 processor |
| [`zonemonitor`](src/zonemonitor.rs) | Zone block monitor and multi-block batch aggregation |
