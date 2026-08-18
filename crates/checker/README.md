# zone-checker

An observe-only bridge-accounting checker that runs as a Reth execution
extension (ExEx) inside a Tempo Zone node. It independently derives token
balances and bridge liabilities from authenticated protocol activity, then
compares them with exact Zone and Tempo state.

See [DESIGN.md](DESIGN.md) for the accounting model and block invariants.

## What it verifies

For every canonical Zone block, the checker independently derives:

- TIP-20 entitlements for every account named by authenticated transfer and
  bridge events.
- Each enabled token's expected Zone supply from those entitlements.
- Portal liabilities for circulating supply, pending deposits, pending
  withdrawals, and pending refunds.
- The recipient and amount of every Inbox mint from its authenticated bridge
  lifecycle event.
- Every user withdrawal's exact TIP-20 debit and burn, including sponsored
  fees.

Canonical Portal creation and enablement events establish which tokens the
checker tracks.

Token enablement is not otherwise modeled: the checker does not compare L1 and
L2 enablement calldata or validate token metadata and Inbox/Outbox issuer roles.

It then reads affected balances and every enabled token's supply from the exact
Zone post-state and reads Portal custody at the exact imported Tempo block. The
required invariants are:

```text
Zone balance(account, token) == derived entitlement(account, token)
Zone totalSupply(token)      == sum of derived entitlements(token)
Portal custody(token)        >= supply + deposits + withdrawals + refunds
```

The checker reads all enabled-token supplies every block and reads balances for
accounts touched by that block. Its durable entitlement ledger carries earlier
accounts forward and verifies their aggregate against total supply. It does not
scan arbitrary Zone storage changes or reread every historical account balance
on every block.

Tempo consensus and execution are trust boundaries. Tempo evidence is bound to
exact canonical block hashes and authenticated receipts, and the checker
independently decodes the relevant Portal events. It does not re-execute Tempo.
Zone post-state is read through Reth's exact-block storage provider.

## Startup and recovery

With `--checker.mode observe`, startup is self-contained:

1. Read the Zone ID, Portal, chain ID, and node data directory from the normal
   node configuration.
2. Read the Tempo anchor, bridge cursors, initial token, and supply from exact
   local Zone genesis state, rejecting a genesis with prior bridge progress.
3. Discover the unique matching `ZoneCreated` through Tempo's log index, then
   authenticate its canonical block, receipts, Zone identity, Portal, and
   initial token.
4. Replay canonical Portal activity from creation through the Zone genesis
   anchor and verify initial Portal collateral.
5. Create `<node datadir>/checker` atomically if it is missing, or validate and
   open it if it exists.
6. Ask Reth to replay canonical Zone history after the durable verified tip.

The database is created in `<node datadir>/checker`; no separate checkpoint
command is required. An existing database is opened only when its Zone, Tempo,
Portal, and creation identity matches the authenticated bootstrap result.

MDBX stores current nonzero account rows, per-token accounting, metadata, the
active finding, and 16,384 exact undo deltas. Old deltas are pruned, so reorg
history is bounded; account storage still scales with the number of nonzero
derived entitlements. Reorgs inside the retained window unwind exactly. A
deeper reorg atomically resets to the authenticated genesis checkpoint and asks
Reth to replay canonical local history.

On normal restart, Reth replays notifications after the last durably verified
Zone block. Each block transition is persisted before the ExEx acknowledges its
height.

## Failure behavior

Temporary Tempo RPC or local-state acquisition failures retry without advancing
or acknowledging the block. A deterministic mismatch records one durable
finding, freezes the verified tip, and continues acknowledging subsequent
notifications while recording how far the unchecked range extends. A reorg
that removes the finding resumes verification from the canonical ancestor.

An unrecoverable checker-local error disables the checker and drains ExEx
notifications so it cannot terminate or stall Zone execution. Observe mode
never changes block execution or consensus behavior; operators must alert on
lag or an active divergence.

The node's existing metrics endpoint exports:

- `reth_tempo_zone_checker_verified_zone_height`
- `reth_tempo_zone_checker_imported_tempo_height`
- `reth_tempo_zone_checker_observed_zone_height`
- `reth_tempo_zone_checker_verification_lag_blocks`
- `reth_tempo_zone_checker_divergence_active`
- `reth_tempo_zone_checker_acquisition_retries_total`
- `reth_tempo_zone_checker_verified_zone_blocks_total`
- `reth_tempo_zone_checker_recovery_rebuilds_total`

At minimum, alert when `divergence_active` is `1`, verification lag continues
to grow, or the verified height stops advancing while the Zone head advances.

## Modes

| Mode | Behavior |
| --- | --- |
| `off` | Default. The checker is not installed. |
| `observe` | Verify, persist findings, expose metrics, and leave Zone execution unchanged. |

Use `--checker.mode observe` or `CHECKER_MODE=observe`. The checker reuses the
node's Tempo RPC URL, Portal address, Zone ID, chain specification, and data
directory. No checker-specific anchor, Portal, or database argument is needed.

## Source organization

```text
src/
  bootstrap.rs       genesis identity and authenticated Portal discovery
  l1/                exact Tempo blocks, receipts, and Portal events
  l2/                Zone events and exact post-state reads
  accounting/        pure account and liability transitions
  persistence/       row-oriented MDBX state and bounded reorg deltas
  runtime.rs         recovery, verification, retry, and divergence handling
  metrics.rs         operational progress and alert metrics
```

## Validation

```bash
cargo test -p zone-checker
cargo clippy -p zone-checker --all-targets -- -D warnings
```

## Local verification lab

The disposable lab starts a pinned Tempo development chain and a
checker-enabled Zone, then waits for the checker to verify the blocks containing
token enablement, deposit, and withdrawal activity:

```bash
just checker-lab-up
just checker-lab-trigger token
just checker-lab-trigger deposit
just checker-lab-trigger withdrawal
just checker-lab-trigger all
just checker-lab-status
just checker-lab-logs zone
just checker-lab-restart-zone
```

Each trigger waits until the checker has verified the Zone block containing the
corresponding activity and fails if a divergence becomes active. The token
scenario confirms that the Portal event adds the token to accounting coverage;
it does not validate token metadata or issuer roles. The `all` scenario runs
token enablement, deposit, and withdrawal sequentially.

The lab uses a 10-block withdrawal batch interval, prints periodic head
progress, and fails after three minutes instead of waiting indefinitely.
Override these defaults with `ZONE_BATCH_INTERVAL_BLOCKS` and
`WITHDRAWAL_WAIT_TIMEOUT_SECS` when needed.

State and logs are kept under `target/checker-lab`. Use
`just checker-lab-down` to preserve them or `just checker-lab-reset` to remove
the complete environment.
