# tempo-zone

Tempo Zone L2 node binary with integrated SP1 prover ExEx.

## Overview

This binary runs a lightweight L2 zone node using reth's node builder infrastructure. It optionally includes the Zone Prover ExEx for generating and submitting SP1 proofs to L1.

## CLI Flags

### Zone Prover Options

| Flag | Env Variable | Default | Description |
|------|--------------|---------|-------------|
| `--zone.prover` | `ZONE_PROVER_ENABLED` | `false` | Enable zone prover ExEx |
| `--zone.mock-prover` | `ZONE_MOCK_PROVER` | `true` | Use mock prover instead of SP1 |
| `--zone.portal-address` | `ZONE_PORTAL_ADDRESS` | - | ZonePortal contract address on L1 |
| `--zone.sequencer-key` | `ZONE_SEQUENCER_KEY` | - | Sequencer private key for L1 txs |

### L1 Connection Options

| Flag | Env Variable | Default | Description |
|------|--------------|---------|-------------|
| `--l1.rpc-url` | `L1_RPC_URL` | - | L1 WebSocket RPC URL for deposit events |

## Environment Variables

```bash
# Required for prover
export L1_RPC_URL="wss://tempo-l1.example.com/ws"
export ZONE_PORTAL_ADDRESS="0x1234567890abcdef1234567890abcdef12345678"
export ZONE_SEQUENCER_KEY="0x..."  # 32-byte private key

# Optional
export ZONE_PROVER_ENABLED="true"
export ZONE_MOCK_PROVER="false"  # Set to false for production SP1 proofs
```

## Example Usage

### Development (Mock Prover)

```bash
tempo-zone node \
  --chain zone-dev \
  --zone.prover \
  --zone.mock-prover \
  --l1.rpc-url wss://localhost:8546 \
  --zone.portal-address 0x5FbDB2315678afecb367f032d93F642f64180aa3 \
  --zone.sequencer-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
```

### Production (SP1 Prover)

```bash
tempo-zone node \
  --chain zone-mainnet \
  --zone.prover \
  --zone.mock-prover=false \
  --l1.rpc-url wss://tempo-mainnet.example.com/ws \
  --zone.portal-address 0x... \
  --zone.sequencer-key $ZONE_SEQUENCER_KEY
```

### Without Prover (Sync Only)

```bash
tempo-zone node \
  --chain zone-mainnet \
  --l1.rpc-url wss://tempo-mainnet.example.com/ws
```

## Architecture

When the prover is enabled, the node:

1. Subscribes to L1 deposit events via WebSocket
2. Processes zone blocks and extracts withdrawals
3. Batches blocks (250ms interval or 100 blocks)
4. Generates proofs (mock or SP1)
5. Submits proof bundles to ZonePortal on L1

See [tempo-zone-exex documentation](../../crates/tempo-zone-exex/README.md) for detailed architecture.

## Security Notes

- **Never commit `ZONE_SEQUENCER_KEY` to version control**
- Use a secrets manager or secure environment for the sequencer key
- The sequencer key must have ETH on L1 for gas fees
