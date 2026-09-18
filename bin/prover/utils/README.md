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
  --attestation-policy prover-attestation-policy.json \
  --output proof.json
```

The policy pins PCR0–2 and limits evidence age; each PCR may list multiple deployment values:

```json
{
  "pcrs": {
    "0": ["<96 lowercase-or-uppercase hex characters>"],
    "1": ["<96 hex characters>"],
    "2": ["<96 hex characters>"]
  },
  "max_age_seconds": 300
}
```

The command authenticates Nitro-attested TLS before sending the witness and only writes successful
responses. The saved batch proof is still verified on-chain during settlement.
