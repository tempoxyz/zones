# Prover input generation

`tempo-zone-prover-utils generate-input` collects and locally validates a Zone batch witness.
`--chain` accepts a local genesis JSON path, inline JSON, or an HTTP(S) URL.
Both `--from-block` and `--to-block` accept a decimal Zone block number or a `0x`-prefixed
32-byte block hash. Numbers and hashes can be mixed; both boundaries are inclusive.

```bash
cargo run --release -p tempo-zone-prover-utils -- generate-input \
  --tempo-rpc-url "$TEMPO_RPC_URL" \
  --zone-rpc-url "$ZONE_RPC_URL" \
  --chain "$ZONE_GENESIS" \
  --from-block "$FIRST_ZONE_BLOCK_HASH" \
  --to-block "$LAST_ZONE_BLOCK_HASH" \
  --output witness.json
```

All Zone reads use the unrestricted Zone RPC, including full blocks, historical state, and
`debug_zoneExecutionWitness`. No private Zone RPC endpoint or authentication key is needed.
The L1 RPC is still required to discover the portal and read settlement state.

Hashes are resolved through the unrestricted Zone RPC. Missing blocks and hashes that do not
match the extracted canonical range are rejected. `--from-block` also accepts a hash when used
with `--zone-block-count`; `--to-block` and `--zone-block-count` remain mutually exclusive.

The range must satisfy SPF batch rules, including withdrawal-batch finalization in its last
block. A settlement's `prevBlockHash` identifies the parent of the first included block, so it
must not be used directly as the inclusive `--from-block` value.

## Replay a submitted batch

To select the complete submitted batch containing a particular Zone block:

```bash
cargo run --release -p tempo-zone-prover-utils -- generate-input \
  --tempo-rpc-url "$TEMPO_RPC_URL" \
  --zone-rpc-url "$ZONE_RPC_URL" \
  --chain https://example.com/genesis.json \
  --block 12345 \
  --output witness.json
```

`--block` accepts a decimal number or a `0x`-prefixed block hash and resolves both
boundaries from the portal's `BatchSubmitted` events. It fails
if the block has not been submitted to Tempo yet, and rejects genesis block 0. Historical
replay requires the unrestricted Zone RPC to retain the relevant blocks, state, and
execution witnesses, and the Tempo RPC to retain batch logs.

`--block` cannot be combined with `--from-block`, `--to-block`, `--zone-block-count`,
or `--wait-timeout`. Without `--block`, the existing range selection defaults to the
blocks after the portal's latest commitment through the current Zone tip.

## Prove a saved witness

```bash
cargo run --release -p tempo-zone-prover-utils -- prove \
  --input witness.json \
  --target "$PROVER_TARGET" \
  --output proof.json
```

The target is a `HOST:PORT` TCP endpoint, such as the Nitro host's TCP-to-vsock proxy.
The command handles request framing and saves the complete successful JSON response, including
`output` and `proofBundle` (`verifierConfig` and the attestation in `proof`). No RPC endpoints,
chain specification, or wallet key are needed. The witness is forwarded to the remote prover
without local replay or conversion to this CLI version's witness schema.

Protocol/version mismatches, missing proofs, and prover errors fail the command without writing
the output file. A saved response is not independently authenticated by the CLI; submit the proof
and its public commitments to the on-chain verifier to check the attestation.
