# Native execution review findings

This ledger records implementation self-review and observed devnet failures.
It is not an independent audit. Protocol activation, migration, private Earn
accounting, lane admission, and proof settlement still need their own reviews.

## F001: exclusive provider borrows overlap during native reentry

- **Severity:** high for execution paths that permit native child calls; memory
  safety finding. No production exploit has been demonstrated.
- **Trigger:** a precompile dispatches another precompile using the same EVM.
- **Invariant:** dispatch must not alias exclusive provider references with the
  mutable host or another active provider invocation.
- **Reproduction:** at EVM2 `897e237fc822cfd7bc03eb946c10b953f1c05a6f`, run
  `cargo +nightly miri test -p evm2 --no-default-features --features std --lib precompile_can_call_another_precompile`.
  Miri reports undefined behavior when the inner invocation borrows the provider.
- **Fix:** EVM2 `ae57faef118ba98f036607ab15c55a1380551c88` retains a shared
  provider handle outside the mutable host and changes execution to `&self`.
  Reth `6c65cba8c058a52f0a92976f6a26bb99024a8fbf` and Tempo
  `869dbcf08ea89001934ad461c2455aaad2d60b5b` adopt that interface.
- **Regression:** the same nested test passes Miri after the fix. The new
  `shared_provider_retains_state_across_reentry_and_releases_idle_mutation` test
  also passes Miri and checks retained state and restored idle mutation access.
  Normal and async EVM2 suites pass (556 and 580 tests respectively).
- **Limits:** Miri's Stacked Borrows model is experimental; existing
  integer-to-pointer casts produce provenance warnings. Inspector and
  type-erasure unsafe paths require separate review. Provider interior borrows
  must be released before reentry.

The [EVM2 draft PR](https://github.com/alloy-rs/evm2/pull/523) contains the
implementation and a checked-in reproducer description.

## F002: unconfigured local prover submits an invalid proof-mode payload

- **Severity:** medium for local development liveness; production proof
  validation correctly rejects the payload.
- **Trigger:** run the sequencer without a configured prover. It previously sent
  verifier configuration `[1]` (execution proof mode) with an empty proof.
- **Invariant:** verifier mode must match the submitted proof representation;
  configured prover failures must not silently fall back to a bypass.
- **Reproduction:** the EVM2 bridge baseline repeatedly returned `InvalidProof`
  before the sequencer was corrected.
- **Fix:** Zones `3c64961cfdb8e31aaed7f16b4b692f056b80e744` sends explicit
  configuration `[2]` and empty proof only for the unconfigured local prover.
  Configured malformed proof bundles return an error.
- **Regression:** sequencer proof-mode tests and the real baseline settlement
  receipts pass. `scripts/native-payments/check-baseline.py` verifies the
  submitted configuration and empty proof directly from transaction calldata.
- **Limits:** explicit NoProof mode is temporary baseline development behavior.
  It supplies no execution-proof security and cannot satisfy the native upgrade
  or final devnet acceptance gates.
