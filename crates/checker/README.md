# Zone checker

`zone-checker` observes one Zone and its imported Tempo blocks. It authenticates
block data, evaluates bridge transitions, compares the resulting effects and
state, and stores progress in a dedicated MDBX database before advancing Reth's
verified watermark.

The checker does not affect consensus. When it finds a mismatch, it commits an
active finding. Descendants remain unverified until a reorg removes the
finding's block.

See [DESIGN.md](DESIGN.md) for the data flow and persistence contract.

## Operator setup

Observe mode automatically builds the initial checkpoint when its database path
is absent. The checkpoint is bound to the local Zone genesis, chain IDs,
Portal, discovered creation block, and imported Tempo block. The explicit
command remains useful for preflight or init-container workflows.

```sh
tempo-zone checker build-checkpoint \
  --checker.database-path /var/lib/tempo-zone/checker/db \
  -- \
  node \
  --chain /etc/tempo-zone/genesis.json \
  --datadir /var/lib/tempo-zone \
  --l1.rpc-url wss://tempo-archive.example \
  --l1.portal-address 0x... \
  --zone.id 123

tempo-zone \
  --checker.mode observe \
  --checker.database-path /var/lib/tempo-zone/checker/db \
  --l1.rpc-url wss://tempo-archive.example \
  --l1.portal-address 0x... \
  --zone.id 123
```

Use `tempo-zone checker build-checkpoint --help` for CLI details. Bootstrap
discovers the matching `ZoneCreated` event from Tempo and authenticates its
containing block before persisting it as the checker identity. Observe mode
requires `--checker.database-path`. The checker is off by default.
`--checker.acquisition-timeout-secs` defaults to 30 seconds.

Checkpoint publication uses a sibling staging directory and validates the
database before moving it to the requested path. An existing database is not
replaced. Build a new checkpoint at another path after a schema change or when
the existing database is invalid. When the parent is a mounted volume, use an
absent child path such as `/var/lib/tempo-zone/checker/db`.

Inspect durable progress and alert state with:

```sh
tempo-zone checker inspect \
  --checker.database-path /var/lib/tempo-zone/checker/db \
  --json
```

## Development lab

The repository includes a persistent local Tempo L1 and Zone checker lab for
testing bridge changes. It requires `cargo`, `cast`, `jq`, and `just`.

The lab uses a sibling `../tempo` checkout by default. That checkout must be at
the revision pinned by the Zones workspace so its native contracts match the
Zone bindings. To keep an existing Tempo checkout on another branch, create a
compatible worktree once and point the lab at it:

```sh
TEMPO_REV=$(sed -nE 's/.*tempo-alloy.*rev = "([0-9a-f]+)".*/\1/p' Cargo.toml | head -n 1)
git -C ../tempo worktree add --detach ../tempo-zone-checker "$TEMPO_REV"
export TEMPO_ROOT="$PWD/../tempo-zone-checker"
```

Start the persistent L1, provision a Zone, and start the Zone in checker
observe mode. The checker builds its missing checkpoint during startup:

```sh
just checker-lab-up
just checker-lab-status
```

Submit representative bridge operations and wait for the checker to verify
them:

```sh
just checker-lab-trigger token
just checker-lab-trigger deposit
just checker-lab-trigger withdrawal
```

Follow the Zone/checker or Tempo L1 logs in another terminal:

```sh
just checker-lab-logs zone
just checker-lab-logs l1
```

The Zone log reports protocol operations at `INFO` only after the checker has
verified and durably recorded the transition. It also includes generic
per-block verification at `DEBUG` under the lab's default log filter.

`checker-lab-status` compares the imported Tempo tip with finalized L1, rather
than the live L1 head. The head-to-finalized distance is expected consensus
finality; `finalized lag` is checker/Zone ingestion lag. Zone `lag` is the
distance from the live Zone head to the verified Zone tip. The included JSON is
the authoritative durable checker state.

After changing checker, Zone ingestion, precompile, or payload code, rebuild and
restart only the Zone while preserving L1, Zone, checkpoint, and checker state:

```sh
just checker-lab-restart-zone
```

Stop the processes while preserving state, or stop them and delete all lab
state:

```sh
just checker-lab-down
just checker-lab-reset
```

State and logs live under `target/checker-lab` by default. Set
`CHECKER_LAB_STATE_DIR` to use another location; set `TEMPO_ROOT` to use another
compatible Tempo checkout.

## What it checks

Each Zone `advanceTempo` must import exactly one Tempo block. The checker fetches
that block's complete ordered transaction envelopes and receipt set. It
reconstructs both roots, checks receipt metadata and the aggregate bloom, and
decodes protocol calldata and events. It compares the kernel result with
receipt events, Zone state, token supply, and Portal collateral.

The kernel covers Portal creation, token enablement, deposits, withdrawals,
batches, bounce-backs, refunds, callbacks, commitments, ownership, and token
accounting. It checks collateral after the Tempo transition and before the Zone
transition. It checks Zone commitments and token supply afterward.

### Example: token enablement

Token enablement illustrates the separation between authenticated observations
and independently derived expectations:

```text
L1 Portal       L2 block        Observation     Adapter         Kernel/runtime
    │               │               │               │               │
    │ TokenEnabled ────────────────▶│               │               │
    │               │ inputs ─────▶│               │               │
    │               │ event ──────▶│               │               │
    │               │               │ strict decode │               │
    │               │               │ and validation│               │
    │               │               ├──────────────▶│               │
    │               │               │               │ compare L1    │
    │               │               │               │ with L2 input │
    │               │               │               ├─────────────▶│
    │               │               │               │ observed event│
    │               │               │               ├─────────────▶│
    │               │               │               │               │ compare expected
    │               │               │               │               │ with observed
    │               │               │               │               ├─▶ accept/finding
```

Event decoding first pins the shared Portal and Inbox event types and validates
metadata bounds. The adapter then requires the ordered L2 event envelope to
belong to the `advanceTempo` transaction. Finally, the kernel requires the full
ordered L1 enablement values—token address, name, symbol, and currency—to equal
the L2 inputs, and the runtime requires the emitted L2 effect to equal the
independently derived expected effect. See
[`observe/events/mod.rs`](src/observe/events/mod.rs),
[`adapter/mod.rs`](src/adapter/mod.rs), and
[`kernel/transition/mod.rs`](src/kernel/transition/mod.rs).

## Durability and runtime behavior

The dedicated database has four tables:

| Table | Purpose |
|---|---|
| `Meta` | Identity, schema, tips, coverage, and active finding |
| `Checkpoints` | Bootstrap state and later state cuts |
| `Journal` | Ordered canonical per-block deltas and continuity data |
| `Findings` | Findings and their chain coordinates |

The bootstrap checkpoint is immutable. The checker retains bootstrap,
recovery, and active checkpoints, plus at least 16,384 Zone blocks of journal
history after the recovery checkpoint. It prunes older journal rows atomically;
normal checkpoint cadence retains at most one additional interval.

ExEx notifications are wakeups, not the replay journal. The runtime records the
local canonical head as its recovery target, then reads each missing canonical
block and receipt set from the local node in order. Reth's `FinishedHeight`
remains at the durably verified tip. Each verified block commits separately.

Verified and observed progress are stored separately. A finding leaves semantic
state at the verified parent. Restart validates retained journal continuity,
then resumes from the verified checkpoint toward the locally retained canonical
head. A reorg within retained history restores the common ancestor and then
applies the replacement branch. A deeper reorg durably blocks the checker
without advancing its verified watermark and requires rebuilding its database.
Removing the finding's block clears the active finding atomically; retaining it
keeps the finding active.

Malformed notifications are not treated as verified.

## Failure policy

- **Immediate terminal:** invalid local identity, schema, or notification data.
- **Retry:** unavailable local or Tempo provider data pauses sequential recovery
  and retries with bounded backoff.
- **Authenticated divergence:** record a finding without advancing past the
  block; do not check descendants.

Temporary provider failures never create a coverage gap. The verified tip stays
at the last authenticated block until recovery succeeds. A coverage gap records
the descendants left unchecked by an authenticated divergence.

### Metrics

The checker publishes alert-oriented metrics through the node's existing metrics endpoint:

- `tempo_zone_checker_divergences_total{category="..."}` increments after a divergence is
  durably recorded. The bounded `category` label identifies the finding family; inspect
  the corresponding checker log for block hashes, code, location, and summary.
- `tempo_zone_checker_state{state="..."}` is a one-hot lifecycle gauge. The bounded states
  distinguish checkpoint bootstrap and opening, Tempo connection and retries, normal
  recovery, complete coverage, divergence, durable blocking, and an unavailable database.
- `tempo_zone_checker_verified_zone_height`, `tempo_zone_checker_observed_zone_height`, and
  `tempo_zone_checker_verification_lag_blocks` expose checker progress. The imported Tempo tip
  and reorg recovery checkpoint are exported as `tempo_zone_checker_imported_tempo_height` and
  `tempo_zone_checker_recovery_checkpoint_height`.
- `tempo_zone_checker_divergence_active`, `tempo_zone_checker_coverage_gap`,
  `tempo_zone_checker_recovering`, and `tempo_zone_checker_blocked` expose durable coverage
  state. Inspect the checker database or its structured logs for the terminal reason.
- `tempo_zone_checker_authentication_duration_seconds` measures complete block authentication
  attempts, including attempts that time out. `tempo_zone_checker_acquisition_retries_total`
  counts attempts retried for unavailable local or Tempo data.
- `tempo_zone_checker_verified_zone_blocks_total` increments only after a Zone transition is
  committed.

Snapshot-derived gauges are restored from durable checker metadata on startup, so a restart
does not clear an unresolved divergence or blocked condition.

For checker-enabled nodes, alert on any increase in `tempo_zone_checker_divergences_total`,
while `tempo_zone_checker_divergence_active == 1`,
`tempo_zone_checker_coverage_gap == 1`, or `tempo_zone_checker_blocked == 1`, and if the
`tempo_zone_checker_state` series is absent. Alert separately when lag remains non-zero without
verified-height progress; recovering history is expected only while that lag is shrinking.

## Trust assumptions and non-claims

The checker verifies imported transaction and receipt commitments. It relies on
the in-process Reth node for the canonical Zone chain and hash-pinned Zone state
reads. It does not verify those reads against a state trie.

The Tempo archive endpoint supplies historical envelopes, receipts, and
hash-pinned Portal balance reads. The checker does not verify balance reads
against a storage trie. Missing history is an acquisition failure, not a zero
value or a successful check.

The checker does not validate encrypted payload cryptography, arbitrary EVM or
callback behavior, private mint recipients, or a fallback recipient that is
not present in the observed data.
