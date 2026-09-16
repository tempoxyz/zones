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
