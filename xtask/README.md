# `tempo-xtask`

A polyfill to perform various operations on the codebase.

Subcommands currently supported:

- `admin`: read-only checks and guarded operational commands for deployed Zones.
  See the [admin command documentation](src/admin/README.md).
- `create-zone`: creates a new Zone through Tempo's native TIP-1091 ZoneFactory.
- `generate-zone-genesis`: generates a Zone L2 genesis file.
- `pause-portal`: pauses new deposits, Zone block production, and L1 withdrawal processing for 30 days.

`create-zone` and `generate-zone-genesis` embed the explicit default schedule in
[`genesis-forks.json`](genesis-forks.json): T0 through T11 activate at timestamp
zero, and later forks are omitted. Updating a dependency does not add new forks
to this schedule.

Pass `--fork-schedule <path>` to replace that schedule with a JSON object mapping
Tempo genesis fields to unsigned 64-bit activation timestamps. Copy the default
file and edit it to match the parent L1; for a T12-only devnet, add `"t12Time": 0`
and leave T13/T14 absent. Any supported Tempo fork can be configured this way
without adding a new CLI flag. On custom parent chains, omitted forks remain
inactive; registered public/local chains still inherit their parent schedule
when the node loads genesis. Ethereum and Zone fork settings are unaffected.

The existing `--t12-time` and `--t13-time` flags override the selected schedule's
entries for compatibility with workflow callers. For example, `--t12-time 0
--t13-time 9223372036854775807` retains the T12-only workflow configuration.
The schedule file is read and validated during CLI parsing, before `create-zone`
can submit a transaction. Unknown field names and invalid timestamps are errors.
These options configure new Zone genesis files only; they do not change the
parent L1 schedule or existing chains.
