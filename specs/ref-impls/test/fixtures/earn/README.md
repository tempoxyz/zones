# Tempo Earn Zone boundary fixtures

These contracts are copied from `tempoxyz/earn` commit
`0e2c6859ef501daf11071a24f0db30e573da084f` for the node integration tests.
They live under `test/fixtures` so Foundry compiles their deployment artifacts without treating the
vendored contracts as production coverage targets.

The production contracts are semantically unchanged and are formatted with this repository's
Foundry configuration. The minimal OpenZeppelin dependency closure is copied from Earn's pinned
`openzeppelin-contracts` revision `e4f70216d759d8e6a64144a9e1f7bbeed78e7079`.

`interfaces/external/tempo/IZone.sol` includes the `accessMode` and `gatewayMode` fields now present
in Zones main's `ZoneInfo`. Earn's current minimal interface predates those fields; without this ABI
compatibility update, `EarnRouter` cannot decode `ZoneFactory.zones()` against current Zones.
