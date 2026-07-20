# tempo-xtask

A polyfill to perform various operations on the codebase.

Subcommands currently supported:

+ `create-zone`: creates a new zone through Tempo's native TIP-1091 ZoneFactory.
+ `configure-benchmark-fees`: initializes portal bridge fee rates and optionally the Zone outbox
  withdrawal fee rate using the sequencer key from the environment.
+ `generate-zone-genesis`: generates a zone L2 genesis file.
+ `install-reference-zone-factory`: installs the EIP-2935 history contract and
  constructor-equivalent reference ZoneFactory, Verifier, and ZoneMessenger state in a generated
  Tempo L1 genesis.
