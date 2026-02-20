# Tempo Zones

Zones are L2 chains anchored to Tempo L1. Each zone has its own sequencer, genesis state, and portal contract on L1 that escrows deposits and processes withdrawals.

**Explorers:** [Moderato](https://explore.moderato.tempo.xyz/) · [Devnet](https://explore.devnet.tempo.xyz/)

## Quick Start (One Command)

The fastest way to deploy and run a zone on moderato:

```bash
export L1_RPC_URL="wss://eng:bold-raman-silly-torvalds@rpc.moderato.tempo.xyz"
just deploy-zone my-zone
```

This single command will:
1. Generate a fresh sequencer keypair
2. Fund the sequencer on L1 via `tempo_fundAddress`
3. Build the Solidity specs
4. Deploy a zone on L1 via ZoneFactory (`createZone`)
5. Generate the zone's `genesis.json` and `zone.json`
6. Build and start the zone node

> ⚠️ **Save your sequencer key** — it's printed before the node starts. You'll need it to restart the node later.

Once running, see [Interact with the Zone](#6-interact-with-the-zone) to deposit, withdraw, and check balances.

To restart the zone later:

```bash
export SEQUENCER_KEY="0x<your-sequencer-private-key>"
just zone-up my-zone false release
```

## Step-by-Step Guide

### Prerequisites

- [Rust toolchain](https://rustup.rs/)
- [Foundry](https://book.getfoundry.sh/getting-started/installation) (`cast`, `forge`)
- [`just`](https://github.com/casey/just#packages)

### 1. Set the L1 RPC URL

All zone commands need an L1 RPC URL.

**Moderato testnet:**
```bash
export L1_RPC_URL="wss://eng:bold-raman-silly-torvalds@rpc.moderato.tempo.xyz"
```

**Devnet:**
```bash
export L1_RPC_URL="wss://eng:bold-raman-silly-torvalds@rpc.devnet.tempoxyz.dev"
```

### 2. Generate a Sequencer Key

The sequencer is the operator that builds zone blocks, processes deposits, and submits batch proofs back to L1.

```bash
cast wallet new
```

Save both the **address** and **private key**.

```bash
export SEQUENCER_KEY="0x<your-private-key>"
SEQUENCER_ADDR=$(cast wallet address "$SEQUENCER_KEY")
```

### 3. Fund the Sequencer on L1

The sequencer needs pathUSD on L1 to pay for the `createZone` transaction and deposit fees.

```bash
cast rpc tempo_fundAddress "$SEQUENCER_ADDR" --rpc-url "$L1_RPC_URL"
```

Verify the balance:

```bash
cast call 0x20C0000000000000000000000000000000000000 \
  "balanceOf(address)(uint256)" "$SEQUENCER_ADDR" \
  --rpc-url "$L1_RPC_URL"
```

View on explorer: `https://explore.moderato.tempo.xyz/address/<SEQUENCER_ADDR>`

### 4. Create the Zone on L1

This deploys a ZonePortal + ZoneMessenger on L1 and generates the zone's genesis file:

```bash
export PRIVATE_KEY="$SEQUENCER_KEY"
just create-zone my-zone
```

This creates `generated/my-zone/` containing:
- **`genesis.json`** — Zone L2 genesis state (system contracts, fee token, etc.)
- **`zone.json`** — Deployment metadata (portal address, zone ID, anchor block)

You can also run the xtask directly for more control:

```bash
cargo run -p tempo-xtask -- create-zone \
  --output generated/my-zone \
  --sequencer "$SEQUENCER_ADDR" \
  --private-key "$SEQUENCER_KEY"
```

### 5. Start the Zone Node

```bash
just zone-up my-zone false release
```

Use `release` profile for production (recommended). Omit it for debug builds during development.

The zone node will:
- Listen on `http://localhost:8546` for JSON-RPC
- Subscribe to L1 for deposit events and backfill from the genesis anchor block
- Build one zone block per L1 block (catches up at full speed during sync)
- Submit batch proofs to L1 every 60s (or immediately when withdrawals are pending)
- Process withdrawals from the zone back to L1

To reset the zone's datadir and start fresh:

```bash
just zone-up my-zone true release
```

The zone node stores data in `/tmp/tempo-zone-<name>/`.

### 6. Interact with the Zone

#### Check balance on the zone

```bash
just check-balance <address>
# or with a custom token and rpc (positional args):
just check-balance <address> <token> <rpc>
```

#### Deposit from L1 to Zone

First, approve the portal to spend your tokens:

```bash
export L1_PORTAL_ADDRESS=$(jq -r '.portal' generated/my-zone/zone.json)
just max-approve-portal
```

Then send a deposit (pathUSD by default):

```bash
just send-deposit 1000000                       # deposit 1M pathUSD to your own address
just send-deposit 1000000 <recipient-address>   # deposit 1M pathUSD to a specific address
```

#### Withdraw from Zone to L1

First, approve the outbox:

```bash
just max-approve-outbox
```

Then request a withdrawal:

```bash
just send-withdrawal 1000000                       # withdraw 1M to your own address
just send-withdrawal 1000000 <recipient-address>   # withdraw 1M to a specific address
```

The sequencer detects the withdrawal, includes it in the next batch submission to L1, and then processes it on L1 automatically.

#### Check balance on Zone L2

```bash
just check-balance <address>
```

#### Check balance via Private RPC

The private RPC (port 8544) requires a signed auth token derived from your private key. This ensures only the account owner can query their own balance.

```bash
# Check your balance (generates auth token automatically)
just check-balance-private my-zone

# Generate a reusable auth token (valid for 10 minutes)
TOKEN=$(just zone-auth-token my-zone)
curl -s http://localhost:8544 \
  -H "Content-Type: application/json" \
  -H "x-authorization-token: $TOKEN" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
```

#### Check portal status on L1

```bash
PORTAL=$(jq -r '.portal' generated/my-zone/zone.json)
HTTP_RPC=$(echo "$L1_RPC_URL" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')

# Last L1 block synced by the zone
cast call "$PORTAL" "lastSyncedTempoBlockNumber()(uint64)" --rpc-url "$HTTP_RPC"

# Withdrawal queue status (head == tail means empty)
cast call "$PORTAL" "withdrawalQueueHead()(uint256)" --rpc-url "$HTTP_RPC"
cast call "$PORTAL" "withdrawalQueueTail()(uint256)" --rpc-url "$HTTP_RPC"
```

## Architecture

```mermaid
graph TB
    subgraph L1["Tempo L1 (Moderato)"]
        Factory["ZoneFactory (shared)"]
        Portal["ZonePortal (per-zone)<br/>deposits · batch proofs · withdrawals"]
        Factory -->|createZone| Portal
    end

    subgraph L2["Zone L2 Node"]
        direction TB
        Tasks["Sequencer Tasks<br/>• L1 subscriber (deposit backfill + live)<br/>• Zone engine (L1-driven block building)<br/>• Zone monitor (batch submission to L1)<br/>• Withdrawal processor (L1 queue drain)"]
        Predeploys["Predeploys<br/>0x1c00…0000 TempoState<br/>0x1c00…0001 ZoneInbox<br/>0x1c00…0002 ZoneOutbox<br/>0x1c00…0003 ZoneConfig<br/>0x1c00…0004 TempoStateReader<br/>0x20C0…0000 pathUSD"]
    end

    Portal -- "WSS subscription<br/>(deposits, headers)" --> Tasks
    Tasks -- "submitBatch / processWithdrawal" --> Portal
```

## Configuration

### Key Addresses

| Contract | Address |
|----------|---------|
| pathUSD (TIP-20) | `0x20C0000000000000000000000000000000000000` |
| ZoneFactory (moderato) | `0xb2D823504d3caa517e9cB42017404f316e0f8312` |

### Zone Node CLI Options

| Flag | Default | Description |
|------|---------|-------------|
| `--l1.rpc-url` | (required) | L1 WebSocket RPC URL |
| `--l1.portal-address` | (from zone.json) | ZonePortal contract on L1 |
| `--l1.genesis-block-number` | (from zone.json) | L1 block when the zone was created |
| `--zone.id` | 0 | Zone ID from ZoneFactory (for private RPC auth) |
| `--sequencer-key` | (optional) | Sequencer private key for block production |
| `--block.interval-ms` | 250 | Block building interval |
| `--zone.batch-interval-secs` | 60 | Max seconds to accumulate zone blocks before submitting a batch to L1 |
| `--zone.poll-interval-secs` | 1 | How often (seconds) the zone monitor polls for new L2 blocks |
| `--withdrawal-poll-interval-secs` | 5 | How often (seconds) the withdrawal processor polls the L1 queue |
| `--http.port` | 8546 | HTTP JSON-RPC port |
| `--private-rpc.port` | 8544 | Private RPC server port |

### Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `L1_RPC_URL` | Yes | L1 WebSocket URL (`wss://...`) |
| `SEQUENCER_KEY` | For sequencing | Sequencer private key |
| `PRIVATE_KEY` | For transactions | Key for L1 transactions (deposits, approvals) |
| `L1_PORTAL_ADDRESS` | For deposits | ZonePortal address (from `zone.json`) |

## Justfile Commands Reference

| Command | Description |
|---------|-------------|
| `just deploy-zone <name>` | One-shot: keygen → fund → create → genesis → start node |
| `just create-zone <name>` | Create zone on L1 + generate genesis (requires `PRIVATE_KEY`, `SEQUENCER_KEY`) |
| `just zone-up <name> [reset] [profile]` | Start the zone node. `reset=true` wipes datadir. `profile=release` for production. |
| `just max-approve-portal` | Approve portal to spend tokens on L1 |
| `just send-deposit [to]` | Deposit tokens from L1 to zone (defaults to sender) |
| `just max-approve-outbox` | Approve outbox to spend tokens on zone |
| `just send-withdrawal [to]` | Withdraw tokens from zone to L1 (defaults to sender) |
| `just check-balance <addr>` | Check token balance on the zone |
| `just zone-auth-token <name>` | Generate a signed private RPC auth token (10 min TTL) |
| `just check-balance-private <name>` | Check balance via the private RPC (auto-generates auth token) |
