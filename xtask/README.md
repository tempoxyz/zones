# `tempo-xtask`

A polyfill to perform various operations on the codebase.

Subcommands currently supported:

- `admin`: read-only checks and guarded operational commands for deployed Zones.
  See the [admin command documentation](src/admin/README.md).
- `create-zone`: creates a new Zone through Tempo's native TIP-1091 ZoneFactory.
- `generate-zone-genesis`: generates a Zone L2 genesis file.
- `pause-portal`: pauses new deposits, Zone block production, and L1 withdrawal processing for 30 days.
