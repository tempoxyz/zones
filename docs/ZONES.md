# Tempo Zones

Zones are L2 chains anchored to Tempo L1. Each zone has its own sequencer, genesis state, and portal contract on L1 that escrows deposits and processes withdrawals.

**Explorers:** [Moderato](https://explore.moderato.tempo.xyz/) · [Devnet](https://explore.devnet.tempo.xyz/)

## Quick Start (One Command)

The fastest way to deploy a zone on moderato:

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
6. Print the sequencer key and instructions to start the node

> ⚠️ **Save your sequencer key** — it's printed at the end. You'll need it to run the zone node.

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
# Convert to HTTP for cast commands
HTTP_RPC=$(echo "$L1_RPC_URL" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')

cast rpc tempo_fundAddress "$SEQUENCER_ADDR" --rpc-url "$HTTP_RPC"
```

Verify the balance:

```bash
cast call 0x20C0000000000000000000000000000000000000 \
  "balanceOf(address)(uint256)" "$SEQUENCER_ADDR" \
  --rpc-url "$HTTP_RPC"
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
export SEQUENCER_KEY="0x<your-sequencer-private-key>"
just zone-up my-zone
```

The zone node will:
- Listen on `http://localhost:8546` for JSON-RPC
- Subscribe to L1 for deposit events
- Build blocks every 250ms (configurable via `--block.interval-ms`)
- Submit batch proofs to L1
- Process withdrawals from the zone back to L1

To reset the zone's datadir and start fresh:

```bash
just zone-up my-zone reset=true
```

### 6. Interact with the Zone

#### Check balance on the zone

```bash
just check-balance <address>
# or with a custom token/rpc:
just check-balance <address> token=0x20C0000000000000000000000000000000000000 rpc=http://localhost:8546
```

#### Deposit from L1 to Zone

First, approve the portal to spend your tokens:

```bash
export L1_PORTAL_ADDRESS=$(jq -r '.portal' generated/my-zone/zone.json)
just max-approve-portal
```

Then send a deposit:

```bash
just send-deposit <recipient-address> amount=1000000
```

#### Withdraw from Zone to L1

First, approve the outbox:

```bash
just max-approve-outbox
```

Then request a withdrawal:

```bash
just send-withdrawal <recipient-address> amount=1000000
```

## Architecture

```
┌─────────────────────────────────────────────┐
│                Tempo L1 (Moderato)          │
│                                             │
│  ┌──────────────┐    ┌──────────────────┐   │
│  │ ZoneFactory   │    │ ZonePortal       │   │
│  │ (shared)      │───>│ (per-zone)       │   │
│  └──────────────┘    │  - deposits       │   │
│                      │  - withdrawals    │   │
│                      │  - batch proofs   │   │
│                      └──────────────────┘   │
└─────────────────────────────────────────────┘
                    │ WSS subscription
                    ▼
┌─────────────────────────────────────────────┐
│              Zone L2 Node                   │
│                                             │
│  Predeploys:                                │
│  0x1c00...0000  TempoState                  │
│  0x1c00...0001  ZoneInbox                   │
│  0x1c00...0002  ZoneOutbox                  │
│  0x1c00...0003  ZoneConfig                  │
│  0x1c00...0004  TempoStateReader            │
│  0x20C0...0000  pathUSD (fee token)         │
└─────────────────────────────────────────────┘
```

## Configuration

### Key Addresses

| Contract | Address |
|----------|---------|
| pathUSD (TIP-20) | `0x20C0000000000000000000000000000000000000` |
| ZoneFactory (moderato) | `0x86A7Ca9816806B59C7172015D04F9C2EF5F5D8E0` |

### Zone Node CLI Options

| Flag | Default | Description |
|------|---------|-------------|
| `--l1.rpc-url` | (required) | L1 WebSocket RPC URL |
| `--l1.portal-address` | (from zone.json) | ZonePortal contract on L1 |
| `--l1.genesis-block-number` | (from zone.json) | L1 block when the zone was created |
| `--sequencer-key` | (optional) | Sequencer private key for block production |
| `--block.interval-ms` | 250 | Block building interval |
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
| `just deploy-zone <name>` | One-shot: keygen → fund → create → genesis |
| `just create-zone <name>` | Create zone on L1 + generate genesis (requires `PRIVATE_KEY`, `SEQUENCER_KEY`) |
| `just zone-up <name>` | Start the zone node |
| `just max-approve-portal` | Approve portal to spend tokens on L1 |
| `just send-deposit <to>` | Deposit tokens from L1 to zone |
| `just max-approve-outbox` | Approve outbox to spend tokens on zone |
| `just send-withdrawal <to>` | Withdraw tokens from zone to L1 |
| `just check-balance <addr>` | Check token balance on the zone |
