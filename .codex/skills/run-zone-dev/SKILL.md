---
name: run-zone-dev
description: Run and validate `tempo-zone dev` against either Anvil in Tempo mode or a native Tempo dev L1. Use when starting local Zone development, comparing Anvil and Tempo behavior, checking provisioning and L1 sync, confirming batch submission, or debugging dev-mode RPC, funding, header, port, datadir, and process issues in this repository.
---

# Run Zone Dev

Use release builds and isolated datadirs and ports. Keep every started process handle and stop only those processes during cleanup.

## Prepare

1. Build the binary:

   ```bash
   cargo build --release --bin tempo-zone
   ```

2. Choose unused L1, zone HTTP, and private RPC ports. The zone WebSocket and P2P ports are `HTTP + 1` and `HTTP + 2`.
3. Use a fresh or previously generated dev datadir. `tempo-zone dev` refuses to wipe a non-empty directory without `zone.json`.

## Run with Anvil

Require Foundry 1.8 or newer, or a nightly build from July 11, 2026 or later. Start Anvil in Tempo mode:

```bash
anvil --version
anvil --network tempo --block-time 1 --host 127.0.0.1 --port 8545
```

Start the zone in a second terminal:

```bash
target/release/tempo-zone dev \
  --l1.rpc-url ws://127.0.0.1:8545 \
  --datadir /tmp/tempo-zone-dev-anvil
```

`tempo_fundAddress` is absent on Anvil. The default dev key already has pathUSD. If another dev account needs funds, set its pathUSD balance before starting the zone:

```bash
cast rpc --rpc-url http://127.0.0.1:8545 \
  anvil_dealTIP20 \
  "$DEV_ADDRESS" \
  0x20C0000000000000000000000000000000000000 \
  1000000000
```

`anvil_dealTIP20` sets the account balance directly without changing total supply.

## Run with a native Tempo dev L1

Prefer an existing Tempo dev endpoint when one is available:

```bash
export L1_RPC_URL=ws://127.0.0.1:8546
cast rpc --rpc-url "$L1_RPC_URL" web3_clientVersion
target/release/tempo-zone dev \
  --l1.rpc-url "$L1_RPC_URL" \
  --datadir /tmp/tempo-zone-dev-native
```

When starting Tempo itself, require a valid Tempo L1 genesis and make the HTTP and WebSocket ports explicit:

```bash
tempo node \
  --chain "$TEMPO_GENESIS" \
  --dev \
  --dev.block-time 1sec \
  --http --http.addr 127.0.0.1 --http.port 8545 --http.api all \
  --ws --ws.addr 127.0.0.1 --ws.port 8546 --ws.api all \
  --datadir /tmp/tempo-dev-l1
```

If no external Tempo genesis is available, validate the native path with the repository's real-L1 integration test:

```bash
cargo test -p zone-node --features cli --test it \
  test_dev_provisioner_replays_initial_token_event -- --nocapture
```

## Validate

1. Read the generated metadata and derive the actual ports and portal:

   ```bash
   jq . /tmp/tempo-zone-dev-anvil/zone.json
   ```

2. Confirm the zone advances and pathUSD exists:

   ```bash
   cast block-number --rpc-url http://127.0.0.1:9545
   cast code 0x20C0000000000000000000000000000000000000 \
     --rpc-url http://127.0.0.1:9545
   ```

3. Inspect the latest zone log. Require continued L1 ingestion and no repeating errors. For a full smoke test, wait for both `Submitting batch` and `Batch submitted to L1`.
4. On Anvil, require normal block ingestion with no false reorg warnings.

## Diagnose

- Provisioning before funding receipts settle indicates a regression in `fund_dev_account`.
- A missing custom initial token indicates the genesis anchor skipped the `createZone` block and its `TokenEnabled` event.
- A `canonical Tempo header hash` error means the L1 reports a different block hash from `keccak256(rlp(TempoHeader))`. Upgrade Foundry first; never rewrite header parents in the Zone subscriber.
- Repeated false reorgs where each new block's `parentHash` differs from the subscriber's sealed previous hash are the same canonical-hashing failure.
- If the node appears stalled, compare the L1 tip, the zone's Tempo block number, and the latest subscriber log before restarting.

## Clean up

Send `SIGINT` to the exact zone and L1 process handles started for the run. Wait for them to exit. Never use broad `pkill` commands, and never remove a datadir outside the explicitly selected `/tmp/tempo-zone-dev-*` path.
