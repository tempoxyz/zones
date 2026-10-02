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

The later selector-aware provider revision is
`1e4dbf287df62e5b24fef4f7dbae32deb14b4fc5`; Tempo and Reth pin that
revision in their native-payment branches.

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

## F003: portal-looking calldata cannot confer payment-lane authority

- **Severity:** high if a prefix-only classifier admits arbitrary code to the
  payment lane; the observed T15 smoke transaction used general capacity in the
  earlier devnet binary.
- **Trigger:** submit canonical `deposit` calldata to a forged portal-prefix
  address, or to an account executing delegated code.
- **Invariant:** only a top-level, factory-registered, initialized portal with
  the exact canonical proxy runtime may receive payment capacity. A child call
  or AA bundle cannot launder unrelated execution into that classification.
- **Fix in progress:** Tempo `0f6f54a45` admits canonical, bounded direct
  deposits as candidates. The native handler records successful identity checks
  before paid child execution, and consensus classifies the transaction only
  after execution confirms that marker. The payload builder uses the executed
  classification for lane counters and invalidates candidates that exceed the
  general limit when native identity fails. The transaction marker resets with
  the transaction-owned call budget.
- **Regression so far:** malformed and oversized candidate ABI tests, actual
  T15 provider success and rollback tests, 242 primitive, 1,092 precompile,
  307 EVM, and 28 payload-builder tests pass; affected clippy checks pass.
- **Devnet regression:** the upgraded Tempo binary's second deposit is the only
  transaction in L1 block 1546. Its receipt used 93,267 gas, and the matched
  payload metric recorded one payment transaction, 93,267 payment gas, and zero
  general gas. Zone credit and backing also reconciled; see
  [the T15 evidence](EVM2_T15_NATIVE_DEPOSIT.md). A forged Zone ID 2 deposit
  reverted with no child calls, while its isolated block used 29,850 general
  gas and zero payment gas.
- **Remaining:** test delegated candidates and saturation, cover AA bundles
  without exposing arbitrary child execution, and review
  all pool and prover classifiers. The new native withdrawal smoke checks a
  forged portal-prefix `processWithdrawals` call: it reverted without child
  calls and charged 27,910 general gas and zero payment gas. The finding is
  not closed.

## F004: direct private TIP-20 transfers are disabled on Zones

- **Severity:** delivery blocker for private transfer and combined Earn/Zone flows.
- **Trigger:** after the T15 fork-boundary run credited 2,000,000 private pathUSD,
  send a signed `transfer(address,uint256)` from the funded account on Zone 1.
  Transaction `0xb8781785fe41ef25e0dcaf1f28d8b4d2a74fcdfbdd549c4c93efbcf15906547b`
  reverted with `Unauthorized()`; no value moved.
- **Cause:** `TIP20Rules::admit` rejects every transfer selector during the
  initial permissioned Zone phase. The upstream token transfer path and fixed
  gas wrapper exist, but this gate prevents user-to-user movement.
- **Fix:** Zones `afbabc891` admits transfer selectors at the inherited T15
  fork while forwarding to the upstream token for allowance and TIP-403 policy
  checks. The wrapper retains fixed precompile gas and error redaction.
- **Regression:** 18 focused precompile tests pass, including allowance,
  balance, fixed-gas, and insufficient-balance cases. A new T15 Zone accepted
  three signed transfers; final private balances sum to L1 portal backing and
  Zone supply. The batch covering the second transfer used 98,842 payment gas
  and zero general gas; see [the checked run](EVM2_T15_PRIVATE_TRANSFER.md).
  The initial 250,000-gas difference aligned with the sender's first nonce;
  a later new-recipient transfer used steady-state gas. Complete privacy and
  policy review across callers and failure paths remains open.

## F005: local Zone fork schedule was absent from the T15 devnet

- **Severity:** migration-test blocker; no protocol compromise shown.
- **Trigger:** inspect the Zone genesis and startup hardfork list from the
  fork-boundary run. `t15Time` was absent even though the L1 genesis scheduled
  T15; Zone execution remained at T14. Re-running `tempo-zone dev` on that
  datadir wiped it and provisioned a new Zone ID, as its CLI contract specifies.
- **Impact:** the archived receipts prove L1 activation and surviving Zone
  custody/settlement, but not a Zone execution upgrade. The original Zone 1
  datadir is no longer available for live checker replay. Its genesis was
  reconstructed from the recorded L1 anchor with the exact archived genesis
  hash, but the current datadir contains the newly provisioned Zone 2.
- **Partial fix:** explicit `--dev.t15-time` writes the local Zone schedule
  at provisioning. A new Zone 3 started with T15 active, transferred funds,
  settled a batch, and restarted with `tempo-zone node` using its saved genesis
  and datadir. A fresh devnet must still run an existing Zone through its own
  scheduled fork with funded pre-fork positions and replay/sync verification.
