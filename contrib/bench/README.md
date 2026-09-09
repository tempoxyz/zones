# Benchmark fork selection

Set the workflow's `tempo-hardfork` input (or `ZONES_BENCH_TEMPO_HARDFORK`
locally) to `t11`, `t12`, or another fork supported by the pinned Tempo revision.
`latest` selects its newest supported fork. Unsupported names fail before
genesis generation; later forks are scheduled outside the benchmark window.

The benchmark measures the selected **Zones checkout's contracts** under those
Tempo execution rules. It does not reproduce the historical mainnet contract
deployment for each fork. Before building Tempo, the workflow compiles the Zones
contracts and runs:

```sh
node contrib/bench/prepare-tempo-runtimes.mjs "$TEMPO_ROOT" crates/contracts/out
```

This embeds the local ZonePortal, Verifier, and ZoneMessenger bytecode into every
runtime set in the benchmark Tempo checkout, including future fork-prefixed
sets. Genesis and fork upgrades therefore install the same contracts. Unknown
source formats or incomplete sets fail preparation instead of silently leaving
an upgrade unpatched. The post-block bytecode checks on both validators remain
mandatory.

Build both the Tempo node and its xtask from that patched checkout. Do not use
the upstream SHA-keyed binary download/upload cache for this build. The L1
snapshot manifest includes the patched runtime source hash alongside the Tempo
revision, selected fork, and local contract hashes, so incompatible snapshots
are rebuilt.

The source transformation can be checked without building the nodes:

```sh
node --test contrib/bench/prepare-tempo-runtimes.test.mjs
```

These tests include a synthetic T13 runtime set to exercise future fork handling;
they do not assert that a given Tempo revision implements T13.
