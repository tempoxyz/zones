# Tempo Earn Zone boundary fixtures

These contracts are copied from the `tempoxyz/earn` revision pinned in
`contrib/bench/earn.lock` for the node integration tests.
They live under `test/fixtures` so Foundry compiles their deployment artifacts without treating the
vendored contracts as production coverage targets.

The fixture set includes the canonical EarnVault, EarnFees, EarnFactory, EarnRouter,
EarnContributionController, ERC-4626 engine, swap adapters, and their interfaces, together with
Bridge's DirectSwapV2, TIP-20 controller and handler, and auth registry from the same revision.
The four Bridge direct-swap files pinned to Solidity 0.8.30 use a compatible `^0.8.30` pragma here
so this repository can compile all reference contracts with its Tempo Solidity toolchain.
The vendored `IZone.ZoneInfo` includes the canonical Zones `accessMode` and `gatewayMode` fields
that are not yet present in the pinned Earn interface, so `EarnRouter` can decode the current
ZoneFactory response.
All other vendored Solidity text retains the pinned repository's formatting; the Zones Foundry
formatter excludes this directory so an unrelated fmt pass cannot rewrite the locked source.

The `simple` mode uses the canonical `MinimalDirectSwapAdapter`; the `stablecoin-dex` mode uses
EarnRouter's built-in StablecoinDEX path with no vault-level swap override.
