# Tempo Earn Zone boundary fixtures

These contracts are copied from `tempoxyz/earn` commit
`5d21954ce16ff6f7536a58fffcc47c0a917c502c` for the node integration tests.

The production contracts are unchanged except that `VaultAdapter` and `FeeMath` import the local
minimal `Math` library. `TestERC1967Proxy` is a test-only deployment helper that lets the copied,
initializer-based `VaultAdapter` run without copying Earn's OpenZeppelin submodule.
