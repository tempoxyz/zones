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

Generate a dump and initialize a **nonexistent** Zone database:

```bash
cargo xtask generate-state-bloat --size 1 --token 0 --out zone-state-bloat.bin \
  --mnemonic 'abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about'
cargo run --bin tempo-zone -- init-from-binary-dump \
  --chain zone-genesis.json --datadir zone-data zone-state-bloat.bin \
  --output-genesis bloated-genesis.json --manifest zone-state-bloat.json
# All later init/node commands must use the generated specification:
cargo run --bin tempo-zone -- init --chain bloated-genesis.json --datadir zone-data
```

The example uses a public test-only mnemonic distinct from the default dev
mnemonic. Choose accounts not already funded in your genesis; the benchmark
workflow generates its own private mnemonic.

`--size` is the target dump size in MiB, not the resulting database size; it is
split across every `--token`. The generator and strict dump reader come from
Tempo. The Zone initializer merges storage into the genesis allocation **before**
normal Reth initialization commits the header, trie, and history. It rejects
non-TIP20 addresses, a genesis without pathUSD code, duplicate slots, conflicts
with nonzero genesis storage, malformed dumps, and every existing datadir
(including block zero).

Token `0` is pathUSD, which Zone genesis deploys. Other token IDs (for example
`--token 0 --token 1 --token 2 --token 3`) are seeded as storage-only accounts at
their `0x20C0…{id}` addresses and stay uninitialized until the portal admin
enables the matching L1 token (`just enable-token <token>`). The Zone inbox then
initializes metadata and roles without touching the seeded supply or balances.
Enable every seeded token and wait for the Zone to process `TokenEnabled` before
sending traffic that uses it. Genesis code for those tokens is rejected so they
cannot become usable before the portal enables them.
Output files are never overwritten. The saved chain specification must be used
on restart; using the original genesis is rejected by normal genesis validation.

The manifest includes dump SHA-256, per-token and total entry counts, database file bytes, import time,
configuration digest, genesis hash, and committed/database state roots. A full
hashed-state traversal verifies the root independently of cached trie nodes, and
the stores are reopened before publishing the success manifest. Failed initialization
may leave a partial datadir/output genesis; use a new disposable path when retrying.

This is synthetic benchmark state: generated balances have no corresponding L1
escrow. Use disposable local databases and keep the generated accounts separate
from accounts used for bridge correctness checks.

The initial in-memory allocation importer is limited to 16 MiB dumps plus chunk
headers. This is a conservative smoke-test guard, **not** a validated Nitro capacity
limit. Larger state needs measured resource limits and potentially a streaming
genesis builder.
