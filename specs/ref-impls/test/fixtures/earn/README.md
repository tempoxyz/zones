# Tempo Earn Zone boundary fixtures

These contracts are copied from `tempoxyz/earn` commit
`f87b6066e4cd0951caf7448ca6a8dfdf01944f75` for the node integration tests.
They live under `test/fixtures` so Foundry compiles their deployment artifacts without treating the
vendored contracts as production coverage targets.

The production contracts are unchanged except that `VaultAdapter` and `FeeMath` import the local
minimal `Math` library. The minimal OpenZeppelin dependency closure is copied from Earn's pinned
`openzeppelin-contracts` revision `e4f70216d759d8e6a64144a9e1f7bbeed78e7079`.
`TestERC1967Proxy` is a test-only deployment helper for the initializer-based `VaultAdapter`.
