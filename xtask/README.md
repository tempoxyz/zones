# `tempo-xtask`

A polyfill to perform various operations on the codebase.

Subcommands currently supported:

- `admin`: read-only checks and guarded operational commands for deployed Zones.
  See the [admin command documentation](src/admin/README.md).
- `create-zone`: creates a new Zone through Tempo's native TIP-1091 ZoneFactory.
- `generate-zone-genesis`: generates a Zone L2 genesis file.
- `generate-state-bloat`: generates a TIP20 binary storage dump using Tempo's
  implementation and CLI options.
- `pause-portal`: pauses new deposits, Zone block production, and L1 withdrawal processing for 30 days.

`create-zone` and `generate-zone-genesis` explicitly write every supported Tempo
fork (T0 through T14, including T1A/T1B/T1C) into Zone genesis, defaulting each
activation timestamp to `0`. Override individual timestamps with `--t0-time`,
`--t1-time`, `--t1a-time`, etc. through `--t14-time` to match the parent L1.
For example, a T12-only devnet must pass `--t13-time 18446744073709551615
--t14-time 18446744073709551615` to defer both later forks. Omitting a flag
activates that fork at genesis, even when another fork is delayed explicitly.
These flags configure Zone genesis only; they do not change the parent L1 schedule.

## Offline Zone state bloat

Generate a dump and import it into a stopped, freshly initialized Zone database:

```bash
cargo xtask generate-state-bloat --size 1024 --token 0 --out zone-state-bloat.bin
cargo run --bin tempo-zone -- init --chain zone-genesis.json --datadir zone-data
cargo run --bin tempo-zone -- init-from-binary-dump \
  --chain zone-genesis.json --datadir zone-data zone-state-bloat.bin
```

`--size` is the target dump size in MiB, not the resulting database size. Token
`0` is pathUSD, which the Zone genesis generator initializes. Only select tokens
already present in that genesis; the importer rejects missing accounts. These
commands directly reuse Tempo's generator and importer. The importer runs with
`ZoneChainSpecParser` and `ZoneNode`, preserving Zone chain validation and database
types, and rejects databases that have advanced beyond block 0.

This is synthetic benchmark state: generated balances have no corresponding L1
escrow. Use disposable local databases and keep the generated accounts separate
from accounts used for bridge correctness checks.

The importer updates storage and trie nodes but does **not** rewrite the genesis
header to commit the new state root. Before proving the first batch, the benchmark
harness must reconcile the genesis header and chain specification with the
imported state. This command support alone does not enable bloated-state Nitro
benchmarks; workflow inputs, genesis reconciliation, and an end-to-end settlement
test remain separate work.
