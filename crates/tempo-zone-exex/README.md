# tempo-zone-exex

Execution Extension (ExEx) for SP1 proof generation and L1 submission in Tempo Zones.

## Overview

The Zone Prover ExEx subscribes to zone chain state notifications and:

1. **Batches blocks** - Accumulates blocks based on time interval (250ms default) or block count limits
2. **Generates proofs** - Creates SP1 ZK proofs for state transitions (or mock proofs for development)
3. **Submits to L1** - Sends proof bundles to the ZonePortal contract on Tempo L1

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           Zone Node                                     │
│                                                                         │
│  ┌──────────────┐     ┌──────────────┐     ┌──────────────────────┐   │
│  │   Batcher    │────▶│    Prover    │────▶│     Submitter        │   │
│  │              │     │              │     │                      │   │
│  │ • Blocks     │     │ • MockProver │     │ • ZonePortal.sol     │   │
│  │ • Deposits   │     │ • Sp1Prover  │     │ • Retry logic        │   │
│  │ • Withdrawals│     │              │     │ • Gas estimation     │   │
│  └──────────────┘     └──────────────┘     └──────────────────────┘   │
│         ▲                                            │                 │
│         │                                            ▼                 │
│  ┌──────────────┐                          ┌──────────────────────┐   │
│  │ Chain State  │                          │    ZonePortal (L1)   │   │
│  │ Notifications│                          │                      │   │
│  └──────────────┘                          │ • submitBatch()      │   │
│         ▲                                  │ • Verifies proofs    │   │
│         │                                  └──────────────────────┘   │
│  ┌──────────────┐                                                      │
│  │ L1 Deposit   │◀──── WebSocket subscription to L1 deposit events    │
│  │ Subscriber   │                                                      │
│  └──────────────┘                                                      │
└─────────────────────────────────────────────────────────────────────────┘
```

## Components

### BatchCoordinator

Accumulates blocks, deposits, and withdrawals into batches:

- **Batch interval**: 250ms (configurable)
- **Max blocks per batch**: 100 (configurable)
- **Deposit tracking**: Maintains deposit queue hashes for L1 deposits
- **Withdrawal extraction**: Parses withdrawal events from block logs

### Prover

Two implementations:

| Prover | Description | Use Case |
|--------|-------------|----------|
| `MockProver` | Returns dummy proofs | Development, testing |
| `Sp1Prover` | Generates real SP1 ZK proofs | Production |

### L1Submitter

Submits batches to the ZonePortal contract:

- Automatic nonce management
- Exponential backoff retry (3 attempts default)
- Gas price estimation
- Transaction confirmation polling

## Configuration

### ZoneProverConfig

```rust
pub struct ZoneProverConfig {
    pub batch_config: BatchConfig,
    pub submitter_config: SubmitterConfig,
    pub use_mock_prover: bool,
    pub initial_state_root: B256,
    pub initial_processed_deposit_hash: B256,
    pub initial_pending_deposit_hash: B256,
    pub initial_withdrawal_queue2: B256,
}
```

### BatchConfig

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `batch_interval` | `Duration` | 250ms | Time between batch flushes |
| `max_blocks_per_batch` | `usize` | 100 | Max blocks before forced flush |
| `outbox_address` | `Address` | `0x0` | ZoneOutbox contract for withdrawals |

### SubmitterConfig

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `portal_address` | `Address` | required | ZonePortal contract on L1 |
| `sequencer_key` | `B256` | required | Private key for signing L1 txs |
| `l1_rpc_url` | `String` | required | L1 RPC endpoint |
| `chain_id` | `u64` | 1 | L1 chain ID |
| `max_retries` | `u32` | 3 | Retry attempts for transient failures |
| `retry_delay` | `Duration` | 1s | Base delay between retries |
| `gas_limit` | `u64` | 500,000 | Gas limit for submitBatch |
| `max_fee_per_gas` | `Option<u128>` | estimated | Override max fee |
| `max_priority_fee_per_gas` | `Option<u128>` | 1.5 gwei | Override priority fee |

## Usage

### Programmatic Installation

```rust
use tempo_zone_exex::{install_exex, ZoneProverConfig, SubmitterConfig};

let config = ZoneProverConfig {
    use_mock_prover: false, // Use SP1 prover
    submitter_config: SubmitterConfig {
        portal_address: "0x...".parse()?,
        sequencer_key: sequencer_private_key,
        l1_rpc_url: "wss://l1-rpc.example.com".into(),
        ..Default::default()
    },
    ..Default::default()
};

node_builder.install_exex("zone-prover", |ctx| async move {
    install_exex(ctx, config, Some(deposit_rx)).await
});
```

### CLI Usage

See [tempo-zone binary documentation](../../bin/tempo-zone/README.md) for CLI flags.

## Data Flow

1. **Block Committed** → BatchCoordinator adds block + extracts withdrawals
2. **L1 Deposit Event** → DepositTracker updates pending queue hash
3. **Batch Threshold** → BatchCoordinator flushes batch to Prover
4. **Proof Generated** → L1Submitter encodes and submits to ZonePortal
5. **L1 Confirmation** → BatchCoordinator updates state roots and queue hashes

## Testing

```bash
cargo test -p tempo-zone-exex
```
