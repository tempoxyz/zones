# Prover input generation

`tempo-zone-prover-utils generate-input` collects and locally validates a Zone batch witness.
Both `--from-block` and `--to-block` accept a decimal Zone block number or a `0x`-prefixed
32-byte block hash. Numbers and hashes can be mixed; both boundaries are inclusive.

```bash
cargo run --release -p tempo-zone-prover-utils -- generate-input \
  --tempo-rpc-url "$TEMPO_RPC_URL" \
  --zone-private-rpc-url "$ZONE_PRIVATE_RPC_URL" \
  --zone-unrestricted-rpc-url "$ZONE_UNRESTRICTED_RPC_URL" \
  --chain "$ZONE_GENESIS" \
  --from-block "$FIRST_ZONE_BLOCK_HASH" \
  --to-block "$LAST_ZONE_BLOCK_HASH" \
  --output witness.json
```

Private Zone RPC authentication uses a fresh ephemeral key by default. It is not saved and does
not need funds. To use a specific RPC identity, pass `--private-key` or set `PRIVATE_KEY`;
the flag takes precedence. Invalid supplied keys are rejected rather than replaced.

Hashes are resolved through the unrestricted Zone RPC. Missing blocks and hashes that do not
match the extracted canonical range are rejected. `--from-block` also accepts a hash when used
with `--zone-block-count`; `--to-block` and `--zone-block-count` remain mutually exclusive.

The range must satisfy SPF batch rules, including withdrawal-batch finalization in its last
block. A settlement's `prevBlockHash` identifies the parent of the first included block, so it
must not be used directly as the inclusive `--from-block` value.

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
