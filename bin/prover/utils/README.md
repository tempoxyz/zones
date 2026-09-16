# Prover utilities

`generate-input` generates a batch witness and validates it locally with SPF.
`--chain` accepts a local genesis JSON path, inline JSON, or an HTTP(S) URL.

To replay the submitted batch containing a particular Zone block:

```sh
cargo run --bin tempo-zone-prover-utils -- generate-input \
  --chain https://example.com/genesis.json \
  --tempo-rpc-url "$TEMPO_RPC_URL" \
  --zone-private-rpc-url "$ZONE_PRIVATE_RPC_URL" \
  --zone-unrestricted-rpc-url "$ZONE_UNRESTRICTED_RPC_URL" \
  --block 12345 \
  --output witness.json
```

Set `PRIVATE_KEY` to the key used to authenticate with the private Zone RPC.
`--block` selects the complete batch from the portal's `BatchSubmitted` events,
including both boundary blocks. It fails if the block has not been submitted to
Tempo yet, and rejects genesis block 0. Historical replay requires the unrestricted
Zone RPC to retain the relevant blocks, state, and execution witnesses, and the
Tempo RPC to retain batch logs and the state needed by the witness.

`--block` cannot be combined with `--from-block`, `--to-block`, or
`--zone-block-count` (and its `--wait-timeout`). Without `--block`, the existing
range selection defaults to the blocks after the portal's latest commitment
through the current Zone tip.
