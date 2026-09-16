# Prover input generation

`tempo-zone-prover-utils generate-input` collects and locally validates a Zone batch witness.
Both `--from-block` and `--to-block` accept a decimal Zone block number or a `0x`-prefixed
32-byte block hash. Numbers and hashes can be mixed; both boundaries are inclusive.

```bash
# PRIVATE_KEY authenticates the private Zone RPC.
cargo run --release -p tempo-zone-prover-utils -- generate-input \
  --tempo-rpc-url "$TEMPO_RPC_URL" \
  --zone-private-rpc-url "$ZONE_PRIVATE_RPC_URL" \
  --zone-unrestricted-rpc-url "$ZONE_UNRESTRICTED_RPC_URL" \
  --chain "$ZONE_GENESIS" \
  --from-block "$FIRST_ZONE_BLOCK_HASH" \
  --to-block "$LAST_ZONE_BLOCK_HASH" \
  --output witness.json
```

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

`prove` logs reading the witness, connecting and sending to the prover, waiting for its response,
validating the response, and writing the proof. It prints phase durations and total elapsed time.

## Verify a saved proof

```bash
cargo run --release -p tempo-zone-prover-utils -- verify \
  --input witness.json \
  --proof proof.json \
  --rpc-url "$L1_RPC_URL"
```

Use the original witness file passed to `prove` and its complete saved response. The command
checks the response's request ID against the witness bytes, derives the canonical Zone portal
caller, and ABI-encodes all native verifier arguments. The RPC chain ID must match the witness's
parent chain ID. This command supports the native verifier ABI with a token-enablement transition.

Verification uses `eth_call` at `latest` with a 30,000,000 gas limit; it requires no wallet key,
sends no transaction, and does not settle a batch. It prints `Proof verified: true` only when the
precompile returns ABI-encoded `true`. A false or malformed result, RPC error, or revert fails
the command.

Like `generate-input`, `verify` logs each phase and prints phase durations and total elapsed time.
Set `--log-filter tempo_zone_prover_utils=debug` to inspect all named verifier arguments.
