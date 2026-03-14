# Zone Commands

## Release-first smoke test

```bash
cargo build --bin tempo-zone --release
```

Create a fresh zone when you need a clean router test:

```bash
just deploy-zone my-zone
```

That recipe creates the zone, stores `sequencerKey` in `generated/my-zone/zone.json`, and starts the node immediately. If you need tighter control, run:

```bash
target/debug/tempo-xtask create-zone --output generated/my-zone --l1-rpc-url "$HTTP_RPC" --sequencer "$SEQUENCER_ADDR" --private-key "$SEQUENCER_KEY"
target/debug/tempo-xtask set-encryption-key --l1-rpc-url "$HTTP_RPC" --portal "$PORTAL" --private-key "$SEQUENCER_KEY"
```

## Start a zone in release

```bash
RUST_LOG=warn just zone-up my-zone false release
```

Health check:

```bash
cast block-number --rpc-url http://localhost:8546
```

Read deployment metadata:

```bash
jq '{zoneId, portal, tempoAnchorBlock, zoneFactory, swapAndDepositRouter, sequencerAddress}' generated/my-zone/zone.json
```

## Router validation flow

Deploy the router:

```bash
just deploy-router my-zone
```

Run the demo with defaults:

```bash
just demo-swap-and-deposit my-zone
```

If you need overrides, pass them positionally because the current recipe treats them as positional args:

```bash
just demo-swap-and-deposit my-zone 100000000 0 http://localhost:8546
```

Direct xtask equivalent:

```bash
target/debug/tempo-xtask demo-swap-and-deposit \
  --zone-dir generated/my-zone \
  --l1-rpc-url "$HTTP_RPC" \
  --zone-rpc-url http://localhost:8546 \
  --private-key "$PRIVATE_KEY" \
  --amount 100000000 \
  --tick 0
```

## Sync debugging

Tail the zone log:

```bash
tail -f /tmp/tempo-zone-my-zone*/logs/immutable/reth.log
```

Useful patterns:

```bash
rg -n "Prepared L1 block deposits|Including advanceTempo|TokenEnabled|DepositProcessed|WithdrawalProcessed" /tmp/tempo-zone-my-zone*/logs/immutable/reth.log
```

When `demo-swap-and-deposit` stalls at token enablement:

1. Get the L1 tx block for the `enableToken` tx:

```bash
cast receipt <tx-hash> --rpc-url "$HTTP_RPC"
```

2. Compare it with the latest processed L1 block in the log:

```bash
tail -n 200 /tmp/tempo-zone-my-zone*/logs/immutable/reth.log | rg "Prepared L1 block deposits|Including advanceTempo"
```

If the zone is still behind the tx block, wait longer or rerun the test with a `release` node.

## Known failure modes

- `swapAndDepositRouter not found`: run `just deploy-router <name>` or pass `--router`.
- Missing sequencer key: read `sequencerKey` from `generated/<name>/zone.json` or set `SEQUENCER_KEY`.
- Timeout waiting for `TokenEnabled`: the zone is usually still catching up.
- Restart crash with `failed to seed transferPolicyId ... Uninitialized`: inspect `crates/tempo-zone/src/l1_state/tip403/cache.rs` and prefer a fresh zone for smoke tests involving temporary tokens.
