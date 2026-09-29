# `tempo-xtask`

A polyfill to perform various operations on the codebase.

Subcommands currently supported:

- `admin`: read-only checks and guarded operational commands for deployed Zones.
  See the [admin command documentation](src/admin/README.md).
- `create-zone`: creates a new Zone through Tempo's native TIP-1091 ZoneFactory.
- `generate-zone-genesis`: generates a Zone L2 genesis file.
- `pause-portal`: pauses new deposits, Zone block production, and L1 withdrawal processing for 30 days.

`create-zone` and `generate-zone-genesis` accept `--t12-time` and `--t13-time`
to write the parent L1's activation timestamps into Zone genesis. Pass the same
values used to configure L1; for example, `--t12-time 9223372036854775807
--t13-time 9223372036854775807` keeps both forks disabled for a devnet run.
Omitting a flag preserves the existing default schedule for that fork. These
flags configure Zone genesis only; they do not change the parent L1 schedule.
