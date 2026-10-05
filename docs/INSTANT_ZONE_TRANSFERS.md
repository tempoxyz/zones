# Instant Zone transfer implementation boundary

The repository contains dormant source components for the instant Zone-to-Zone protocol:

- canonical intent, quote, certificate, settlement, and peer-message types in
  `zone-primitives`;
- quorum validation, admission accounting, durable delivery records, terminal transfer state,
  and two-leg replenishment records in `zone-fast-transfer`;
- native escrow and funded-pool execution at the `FastTransfer` precompile address;
- a `SameAnchor` payload/executor/SPF representation that does not repeat L1 import effects; and
- an OpenRaft node adapter that binds committed outcomes to their original term/index and fences
  signing and canonical reads behind durable replay state.

## Activation

There is intentionally no CLI activation flag. The Tempo revision pinned by this repository does
not provide the coordinated hardfork, factory registry, native compatibility pin, or verifier
format required by the protocol. Consequently:

- `ZoneChainSpec::supports_same_anchor()` returns false;
- the local `ZonePortal.FAST_PROTOCOL_NATIVE_PIN` is zero;
- `FastTransfer` state-changing calls reject before activation; and
- the node cannot construct a `FastActivation` capability.

Activation must be one coordinated dependency update that pins the L1 factory and portal
bytecode, Tempo hardfork, native precompile ABI, SPF/prover format, and RPC clients. That update
must also supply finalized three-member epoch evidence and install persistent OpenRaft storage and
authenticated member transport before any fast block can be admitted.

Legacy execution, bridge behavior, and leader authority are unchanged while this fence is closed.
