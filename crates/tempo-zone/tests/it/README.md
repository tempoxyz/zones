# Zone E2E Test Harness

End-to-end test infrastructure for the Tempo Zone node, supporting both fast
synthetic injection and real in-process L1 integration.

## Architecture

The harness provides two independent testing paths:

```
┌─────────────────────────────┐     ┌──────────────────────────────┐
│  Injection Path (e2e.rs)    │     │  Real L1 Path (l1_e2e.rs)    │
│                             │     │                              │
│  L1Fixture builds synthetic │     │  L1TestNode (Tempo dev mode) │
│  TempoHeaders + Deposits    │     │  produces real blocks @500ms │
│           │                 │     │           │                  │
│           ▼                 │     │           ▼                  │
│  DepositQueue.enqueue()     │     │  L1Subscriber (WS + HTTP)    │
│  + seed_l1_cache()          │     │  parses DepositMade logs     │
│                             │     │           │                  │
└───────────┬─────────────────┘     └───────────┬──────────────────┘
            │                                   │
            └───────────────┬───────────────────┘
                            ▼
                    ┌───────────────┐
                    │ DepositQueue  │
                    └───────┬───────┘
                            ▼
                    ┌───────────────┐
                    │  ZoneEngine   │  (pops L1 blocks, builds L2 blocks)
                    └───────┬───────┘
                            ▼
              ┌─────────────────────────┐
              │  Zone L2 Predeploys     │
              │  TempoState  (0x1c..00) │  slot 0=blockHash, slot 7=packed fields
              │  ZoneInbox   (0x1c..01) │  advanceTempo → mint pathUSD
              │  ZoneOutbox  (0x1c..02) │  finalizeWithdrawalBatch
              │  StateReader (0x1c..04) │  reads L1 storage via cache
              └─────────────────────────┘
```

### Injection Path (`e2e.rs`)

Uses `L1Fixture` to manually construct `TempoHeader` and `Deposit` objects,
push them into the `DepositQueue`, and seed the `SharedL1StateCache` for
`TempoStateReader` precompile reads. Fast (~1s per test) and deterministic.

```rust
let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;
let deposit = fixture.make_deposit(sender, recipient, amount);
fixture.inject_deposits(&zone.deposit_queue, vec![deposit]);
// poll for balance change...
```

**L1Fixture internals:**
- Chains `parent_hash = keccak256(rlp(prev_header))` to match `TempoState` verification
- Monotonic block numbers starting from 1, timestamps from 1,000,000
- `seed_l1_cache()` populates portal storage slots (sequencer=0, deposit_queue_hash=4)
  so `TempoStateReader` precompile reads succeed without a real L1

**Multi-zone support:** Use `next_block()` + `enqueue()` to broadcast the same
`FixtureBlock` to multiple zone deposit queues:

```rust
let b1 = fixture.next_block();
fixture.enqueue(&b1, &zone1.deposit_queue, vec![deposit_for_zone1]);
fixture.enqueue(&b1, &zone2.deposit_queue, vec![]);
```

### Real L1 Path (`l1_e2e.rs`)

Starts an in-process Tempo L1 dev node via `L1TestNode::start()`, then connects
a zone node via `ZoneTestNode::start_from_l1()`. The `L1Subscriber` receives
real blocks over WebSocket.

**Genesis patching in `start_from_l1()`:**

The zone's `TempoState` genesis must be anchored to the L1's current state.
`start_from_l1()` fetches the L1's latest header and patches `zone-test-genesis.json`:

1. **Slot 0** (`tempoBlockHash`): Set to `keccak256(rlp(l1_header))`
2. **Slot 7** (packed `uint64` fields): Low 64 bits set to `l1_header.number`
   - Layout: `(tempoBlockNumber:u64, tempoGasLimit:u64, tempoGasUsed:u64, tempoTimestamp:u64)`
   - Only `tempoBlockNumber` is currently patched; other fields retain genesis defaults

### Portal Address Requirement

`ZoneNode::new()` now rejects `Address::ZERO` for `portal_address`. Local test
helpers use a non-zero dummy portal address instead, and patch the default test
genesis so `ZoneInbox` and `ZoneConfig` read from that dummy portal rather than
the zero address.

## Test Inventory

### `e2e.rs` — Injection-Based Tests

| Test | What it exercises |
|------|-------------------|
| `test_deposit_via_queue_injection` | Single deposit → pathUSD mint on L2 |
| `test_multiple_deposits_across_blocks` | Multi-block, multi-recipient deposits |
| `test_empty_l1_blocks_advance_zone` | Chain continuity without deposits |
| `test_two_zones_independent_deposits` | Cross-zone isolation (shared L1 timeline, independent queues) |
| `test_tempo_state_advances_with_l1_blocks` | `tempoBlockNumber` and `tempoBlockHash` tracking |
| `test_zone_inbox_events_on_deposit` | `TempoAdvanced` + `DepositProcessed` event emission |
| `test_withdrawal_batch_finalization` | `ZoneOutbox.withdrawalBatchIndex` advancement |
| `test_large_deposit_batch` | 10 deposits in one L1 block |

### `l1_e2e.rs` — Real L1 Integration Tests

| Test | What it exercises |
|------|-------------------|
| `test_zone_advances_with_real_l1` | Full L1Subscriber → DepositQueue → ZoneEngine pipeline |
| `test_deposit_via_real_l1` | Zone startup from real L1 + initial state verification |

### `deposit.rs` — Testnet Integration Tests (ignored by default)

| Test | What it exercises |
|------|-------------------|
| `test_l1_deposit_mints_on_zone` | Real deposit on testnet portal → mint on local zone |

## Key Types

- **`ZoneTestNode`** — In-process zone L2 node with RPC endpoint. Fields are
  private; use `http_url()`, `deposit_queue()`, `l1_state_cache()` getters.
  Constructed via `start_local()`, `start_from_l1()`, or `start()`.
- **`L1TestNode`** — In-process Tempo L1 dev node. Fields are private; use
  `http_url()` and `ws_url()` getters. Constructed via `start()`.
- **`L1Fixture`** — Synthetic L1 block builder maintaining hash chain continuity.
- **`FixtureBlock`** — Clonable L1 block for multi-zone broadcast.
- **`poll_until`** — Generic async condition poller with timeout.

## Remaining Work (Task 245)

Full `ZoneFactory` deployment + deposit-through-portal on local L1:

1. Add Rust `sol!` bindings for `ZoneFactory` (or load Foundry artifacts)
2. Deploy `ZoneFactory` from the dev account on `L1TestNode`
3. Call `createZone()` → capture the deployed portal address
4. Start `ZoneTestNode` with the real portal address and anchor block
5. Perform a real deposit: `pathUSD.approve()` + `portal.deposit()`
6. Verify pathUSD mint on L2 + `DepositProcessed` event
7. Test full withdrawal finalization and L1 processing cycle

## Known Issues / Improvements

- **Chain ID collisions:** `start_local()` hardcodes `chain_id = 1337`. Tests
  running in parallel can collide. Use `start_local_with_chain_id()` with unique
  IDs, or switch `start_local()` to pick random chain IDs.
- **Slot 7 partial patch:** `start_from_l1()` only patches `tempoBlockNumber` in
  the packed slot 7. Should also patch `tempoGasLimit`, `tempoGasUsed`, and
  `tempoTimestamp` from the anchor header for full consistency.
- **Event assertions:** Some tests query events from block 0 and assume ordering.
  Filter by sender/recipient/amount for robustness.
