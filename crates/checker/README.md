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
checker tracks. Membership in the accounting token map is the authenticated
enablement record; other accounting effects cannot introduce tokens.

Beyond establishing membership, token enablement is not otherwise modeled: the
checker does not compare L1 and L2 enablement calldata or validate token metadata
and Inbox/Outbox issuer roles.

The accounting model also does not correlate individual L1 deposits with L2
outcomes by deposit hash or authenticate cross-layer withdrawal batch identity.
Those protocol events are still strictly decoded, but only value movements that
affect the aggregate solvency model are retained. Genesis bridge cursors are
checked only to establish the empty bootstrap baseline.

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

Tempo consensus and execution are trust boundaries. For live blocks, the checker
reuses Portal logs whose receipts and headers the node's L1 subscriber already
authenticated. It independently decodes those logs and verifies the exact block
and parent coordinates. Bootstrap and old recovery ranges fall back to fetching
and authenticating the same evidence from the configured archival Tempo RPC. It
does not re-execute Tempo. Zone post-state is read through Reth's exact-block
storage provider.

Checker observe mode requires unpruned account and storage history. Node startup
rejects state-pruning configuration because every restart reauthenticates Zone
genesis and catch-up reads exact historical post-state. Other pruning segments
remain compatible with the verified-height watermark. A data directory whose
state history was already pruned must be resynced without state pruning.

## Startup and recovery

With `--checker.mode observe`, startup is self-contained:

1. Read the Zone ID, Portal, chain ID, and node data directory from the normal
   node configuration.
2. Read the Tempo anchor, bridge cursors, initial token, and supply from exact
   local Zone genesis state, rejecting a genesis with prior bridge progress.
3. Discover the unique matching `ZoneCreated` through Tempo's log index, then
   authenticate its canonical block, receipts, Zone identity, Portal, and
   initial token.
4. Require the genesis anchor to precede Portal creation and initialize empty
   accounting. Normal Zone replay then derives token membership when it reaches
   the creation block.
5. Create `<node datadir>/checker` atomically if it is missing, or validate and
   open it if it exists.
6. Walk canonical Zone blocks and receipts directly after the durable verified tip.

The database is created in `<node datadir>/checker`; no separate checkpoint
command is required. An existing database is opened only when its Zone, Tempo,
Portal, and creation identity matches the authenticated bootstrap result.

MDBX stores current nonzero account rows, per-token accounting, metadata, and
the active finding. Storage therefore scales with the current number of nonzero
derived entitlements rather than Zone history. If the saved checker tip is
absent from local Zone history after local storage replacement or recovery, the
checker atomically resets to authenticated genesis and replays local history.
Zones are append-only, so a live Reth reorg or revert notification is an
invariant violation that disables verification rather than rewriting checker
history.

On normal restart, the checker resumes from the block after its last durably
verified Zone block. Each block transition is persisted before the ExEx reports
that verified height as finished.

## Failure behavior

Temporary Tempo RPC or local-state acquisition failures retry without advancing
or acknowledging the block. Initial connection, bootstrap, and each block's
verification have deadlines. Individual Tempo RPC requests are bounded so an
accepted request that never answers becomes retryable; there is no
separate limit on the number of semantic retry attempts. Tempo retries use
exponential backoff, while unavailable local Zone state retries once per second.
Pruned state disables immediately because it cannot recover. Alloy owns the
established WebSocket lifecycle: it detects a dropped connection, reconnects
with backoff, and replays in-flight requests. The checker reuses that provider
while retrying unavailable Tempo data and retryable RPC responses under the
enclosing bootstrap or block deadline. If Alloy exhausts its finite reconnect
budget, restarting the node creates a fresh provider and resumes from the last
durably verified Zone block.

While bootstrap or block verification is waiting, the checker continues
consuming ExEx notifications without acknowledging unverified heights. It keeps
only the latest delivered block reference and drops the full notification. It
then walks canonical blocks and receipts directly from the local provider, one
block at a time, from `verified + 1` to that captured tip. This avoids both
buffering notification payloads and asking Reth to execute a second backfill.

Zone history is append-only. Reorg and revert notifications disable verification
immediately. Before persisting an observed tip, the checker confirms that it is
still canonical; a missing or changed hash is a fatal invariant violation, not a
recoverable fork. Each directly loaded block must also extend the durable
verified hash.

A deterministic mismatch records one durable finding, freezes the verified tip,
and continues draining subsequent notifications while recording how far the
unchecked range extends. It does not report those unverified heights as finished.
A finding remains active until the checker is rebuilt from authenticated genesis.

An expired deadline, append-only invariant violation, or other unrecoverable
checker-local error disables the checker. It releases the latest
delivered notification payloads by continuing to drain the ExEx stream, but
keeps `FinishedHeight` at the last durably verified block. This prevents pruning
the history needed for a restart; disk usage can therefore grow while the
checker is disabled or diverged. Verification stays disabled for the life of the
process. Restarting the node attempts to resume from the last durably verified
Zone block, and disables with an explicit error if required history was already
pruned. Observe mode never changes block execution or consensus behavior;
operators must alert on lag or an active divergence.

The node's existing metrics endpoint exports:

- `reth_tempo_zone_checker_verified_zone_height`
- `reth_tempo_zone_checker_imported_tempo_height`
- `reth_tempo_zone_checker_observed_zone_height`
- `reth_tempo_zone_checker_verification_lag_blocks`
- `reth_tempo_zone_checker_divergence_active`
- `reth_tempo_zone_checker_disabled`
- `reth_tempo_zone_checker_acquisition_retries_total`
- `reth_tempo_zone_checker_verified_zone_blocks_total`
- `reth_tempo_zone_checker_recovery_rebuilds_total`

At minimum, alert when `disabled` or `divergence_active` is `1`, verification
lag continues to grow, or the verified height stops advancing while the Zone
head advances.

### Verified activity logs

After a Zone block is durably verified, the checker emits structured
`zone::checker` logs for authenticated bridge activity. Zero-value Portal
refund claims are accounting no-ops and are omitted. Within that block,
`authenticated` denotes canonical protocol evidence, `accounted` denotes a
ghost-liability change, and `verified` denotes additional reconciliation
against TIP-20 movements and exact post-block state.

These fields form the stable schema for log-backed dashboards:

- `activity_schema_version`: currently `1`.
- `activity_event`: the stable event name from the table below.
- `activity_source`: `tempo` for Portal activity or `zone` for Zone activity.
- `activity_id`: `v<schema_version>:<zone_hash>:<activity_source>:<activity_index>`,
  which remains stable if recovery replays the same canonical block under the
  same schema.
- `activity_index`: the event's canonical order within its source for the Zone
  block.
- `zone_block`, `zone_hash`, `tempo_block`, and `tempo_hash`: exact verified
  coordinates.
- Event-specific fields such as `token`, `recipient`, `sender`,
  `deposit_number`, `withdrawal_index`, and `callback_success`. Monetary
  `amount` and `fee` fields are decimal strings for both Tempo and Zone values.

| `activity_event` | Meaning |
| --- | --- |
| `portal_deposit_accounted` | Authenticated Portal deposit entered accounting. |
| `portal_token_enabled` | Authenticated Portal token entered accounting coverage. |
| `portal_withdrawal_processed` | Authenticated Portal withdrawal and callback result. |
| `portal_withdrawal_bounce_back` | Authenticated Portal withdrawal bounce-back entered accounting. |
| `portal_deposit_bounce_back` | Authenticated processed Portal deposit bounce-back entered accounting. |
| `portal_deposit_bounce_back_pending` | Authenticated pending Portal deposit bounce-back entered accounting. |
| `portal_refund_accounted` | Authenticated Portal refund entered accounting. |
| `zone_deposit_minted` | Verified Zone deposit mint. |
| `zone_deposit_failed` | Authenticated failed Zone deposit. |
| `zone_deposit_bounce_back_requested` | Authenticated Zone deposit bounce-back request. |
| `zone_withdrawal_burned` | Verified user withdrawal debit and burn. |
| `zone_withdrawal_bounce_back_minted` | Verified Zone withdrawal bounce-back mint. |
| `zone_withdrawal_bounce_back_pending` | Authenticated pending Zone withdrawal bounce-back. |
| `zone_refund_minted` | Verified Zone refund mint. |

`activity_id` lets a log consumer deduplicate replayed activities, but logs are
still retention-bound diagnostic evidence rather than the canonical accounting
ledger.

## Modes

| Mode | Behavior |
| --- | --- |
| `off` | Default. The checker is not installed. |
| `observe` | Verify, persist findings, expose metrics, and leave Zone execution unchanged. |

Use `--checker.mode observe` or `CHECKER_MODE=observe`. The checker reuses the
node's authenticated L1 observations, Tempo RPC URL, Portal address, Zone ID,
chain specification, and data directory. No checker-specific anchor, Portal, or
database argument is needed.

## Source organization

```text
src/
  bootstrap.rs       genesis identity and authenticated Portal discovery
  l1/                shared/fallback Tempo evidence and Portal event decoding
  l2/                Zone events and exact post-state reads
  accounting/        pure account and liability transitions
  persistence/       row-oriented MDBX state and exact verified coordinates
  runtime.rs         recovery, verification, retry, and divergence handling
  telemetry.rs       operational metrics and verified-activity logs
```

## Validation

```bash
cargo test -p zone-checker
cargo clippy -p zone-checker --all-targets -- -D warnings
```
