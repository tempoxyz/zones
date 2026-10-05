# Instant Zone transfer acceptance status

## Current request scope

The requester selected the existing T14 boundary and waived the ten-Zone E2E suite in the
[follow-up request](https://tempoxyz.enterprise.slack.com/archives/C0A87C21805/p1791165862088099).
All other protocol implementation and tests remain in scope. The sections below record the
previous draft's validation boundary; they are not current-turn implementation acceptance.
The T14 implementation and fresh independent tests are in progress.

This worktree is not yet the completed instant-transfer protocol. Concrete OpenRaft execution,
Commonware transport, private RPC, native escrow, real replenishment providers and T14 batch
boundaries are implemented in source; the integrated build, native drain and checkpoint handoff
are still being corrected and validated. No runtime replenishment, chaos, or throughput result is claimed. The source specification
is attached to the [implementation request](https://tempoxyz.enterprise.slack.com/archives/C0A87C21805/p1791158720260559).
Its latency figures are measurement objectives, not hard acceptance cutoffs for this request.

## T14 protocol decision

The requester selected T14 as the coordinated opt-in boundary. Same-anchor opening validation,
ordinary TIP-20 transfers, native fast-transfer registration, and the exact nonzero protocol pin
therefore activate at T14 while pre-T14 behavior remains unchanged. Proof-required settlement is
an explicit fail-closed configuration. Operator-attested settlement is a separately enrolled mode,
never an implicit fallback for missing or invalid proof-required evidence.

All direct Tempo dependencies are pinned to
[`eee71306b9192cb0bd02b4a536e4f2a1040bffc3`](https://github.com/tempoxyz/tempo/commit/eee71306b9192cb0bd02b4a536e4f2a1040bffc3),
published in [Tempo #8118](https://github.com/tempoxyz/tempo/pull/8118). This includes typed finalized
epoch storage, native factory dispatch and the matching T14 Portal runtime. The embedded runtime
was byte-for-byte compared with the compiled Solidity source: 37,935 bytes with hash
`0x8395e7f34efb85d08826ec2e0f56653aae29cbaa4504f5caea337a21052ae258`.
This is source/build verification, not a production deployment or activation.

The factory and Solidity checkpoint gates now require the exact accepted final height/hash,
nonzero Raft log index and nonzero state root. The matched source passed 17 native factory tests,
15 Solidity fast-epoch tests and the T14 runtime/storage-preservation upgrade test. These results
do not replace current combined-node and independent recovery validation.

## Implementation boundaries

| Requirement | Draft work | Remaining acceptance prerequisite |
| --- | --- | --- |
| C1: fork and same-anchor execution | T14 payload/executor/SPF, TIP-20 forwarding, durable 500 ms / 1 MiB boundary scheduler and actual finalization transaction | Integrated boundary/import/policy/replay tests |
| C2: durable quorum commitment | OpenRaft, fsynced log/state/snapshot, canonical execution, authenticated replica network and durable witness signing | Execute the independently authored three-replica fault tests and remaining recovery coverage |
| C3: native escrow and pools | Shared identity, typed anchored epoch and asset checks, certificate and accepted-ancestry retirement | Correct and rerun closed-epoch lock inclusion and current native acceptance fixtures |
| C4: direct operator protocol | Dedicated encrypted Commonware network, signed request/channel/stream binding, durable ingestion and private RPC assembly | Integrated build, reconnect/queue recovery and live authority refresh |
| C5: recovery and admission | Bounded inventories, signed nine-peer retirement ABI, persistent drain/provider adapters | Finish canonical source-history projection, independently checked peer signatures, inbound chunking and exact checkpoint installation/runtime wiring |
| C6: replenishment | Real Alloy source withdrawal, L1 treasury and encrypted deposit providers; durable nonce/action/replacement/fee/net-credit reconciliation | Execute real two-leg recovery, fee, bounce/refund and unchanged-recipient tests |
| C7: ten-Zone proof of behavior | Removed from this request by explicit user direction | Not an acceptance requirement for this implementation turn |

The Portal registry ABI now binds the exact enrolled
peer identities, use explicit duplicate markers, authenticate all lock-watermark/unresolved roots,
prove resolution and final settlement, and require installation of the next roster's checkpoint.
It must retain historical keys and prevent all legacy authority paths from replacing the committed
prefix. A count of arbitrary peer barriers cannot prove retirement is safe.

The transport and native ABI now decode the same bounded `zone-primitives` canonical intent and
certificate bytes and derive transfer identity only through those shared pure types. Literal
cross-language and actual precompile-dispatch vectors remain required acceptance coverage.

## Independent acceptance tests

A fresh author derived expectations from the specification and immutable baseline fixtures rather
than the implementation's transition branches. The draft contains:

- a literal balance ledger with exhaustive reachable schedules through twelve operations and
  seeded longer schedules; this validates the oracle, not actual token execution;
- mutation tests for every transport intent/certificate field, quorum membership, signer
  duplication, epoch/domain separation, bounded decoding and literal wire vectors;
- admission, reserve, funded-liquidity, monotonic disposition, delivery acknowledgment, retry and
  replenishment-record boundary tests;
- journal reopen/snapshot tests for signing records, transfer identity and permanent replay keys;
- same-anchor encoding and timestamp boundary assertions; and
- a real factory test that creates ten unique Portals and then fails explicitly at the missing
  shared-L1, three-replica fast fixture. It is not ignored or counted as successful E2E.

The full required suites still outstanding are:

1. Actual atomic token effects across every lock/payment/rejection/cancellation/crash ordering,
   source certificate delivery and policy-blocked release/refund.
2. Cross-language EVM/transport vectors and source-release receipt inclusion/accepted-ancestry
   retirement, including forged/wrong/pre-release/duplicate evidence.
3. Every Raft/log/fsync/commit/execution/signing/snapshot/database boundary, missing replica,
   old snapshot, split leader and external certified-prefix recovery.
4. Import → same-anchor blocks → import with once-only deposit/supply effects, pause, policy,
   hardfork/key changes, follower/SPF root agreement, stale-anchor stop and historical replay.
5. Every queue/journal/count/value/concurrency limit, reserved recovery service, nine-peer epoch
   closure barriers, delayed locks, drained retirement and retained historical keys.
6. Exact real-deposit A→B→C/local-spend balances before source settlement; both real L1
   replenishment legs, per-Portal backing, unchanged recipients and zero/nonzero fee ledgers.
7. All ninety routes and the ten-Zone ring; three seeded fifteen-minute 100 TPS trials;
   background proof/replenishment and one-replica-offline availability trials. Report p50/p95/p99,
   maximum, timeout/rejection/completion counts and all late samples without hard latency gates.
8. Full payment-boundary crashes, replay/asymmetric partitions, entire Zone outage, liquidity
   races/operator withdrawals, timeout/cancellation/expiry, L1 ambiguity/replacement/bounce/refund,
   disk/snapshot/witness/queue loss, clock/policy/key/roster changes and proof-service outage.
9. Legacy bridge/encrypted-deposit/refund/FIFO/privacy/TIP-403/supply/hardfork regressions and
   proof-required rejection of missing or invalid proofs without `NoProof` fallback.

Every real failure run must retain the topology, seed, minimized message/election schedule,
transaction hashes, certificate bytes, block hashes, pinned binaries, fsync configuration and
resource/timing observations. Recovery timing starts only after the specified prerequisites heal.

## Historical validation evidence (before the current T14 integration)

The initial bare-metal snapshot passed all 28 `zone-fast-transfer` tests: 12 source-local units,
2 independent model tests, 5 certificate tests, 7 protocol boundary tests and 2 journal acceptance
tests. Strict nightly Clippy then found an oversized journal enum variant; boxing that variant
preserves its on-disk encoding. A separate fresh validation agent reruns after corrections.

The fresh independent reviewer subsequently reran the then-current 30 fast-transfer tests successfully
(14 source-local and 16 acceptance tests) and passed the workspace formatting check. It rejected
full E2E acceptance, confirming the earlier dormant execution and divergent bindings plus absent
replica/worker assembly and missing actual spend/replenishment/chaos coverage. The first two gaps
have since been implemented in source; the latter integration gaps remain. Strict Clippy found
redundant test clones, which were removed without changing any assertions.

A second fresh runner then passed all 30 package tests, strict nightly package Clippy with warnings
denied and workspace formatting. This does not change the independent rejection of full E2E.

Foundry 1.8.3 was checksum-verified before use. The Solidity source build passed with Solc 0.8.35.
Rendered storage packs `fastEpoch` at slot 28 offset 9 and the new mappings at slots 29–34. These
are source-rendered results, not proof that matching runtime bytes are installed on Tempo L1.

Full node compilation and node integration test compilation/execution remain unverified. The
remaining broad dependency builds were stopped at the protocol-owner decision boundary; this is
not a successful node/prover/E2E result. Source work is preserved in the draft, and the idle
bare-metal build box was destroyed after its package-test evidence was collected.

There is no accepted ten-Zone E2E result, and it is not required by the revised scope. Do not claim
complete protocol readiness from unit, model, journal, source compilation, or Solidity results;
the remaining node and bridge integration blockers above are material.
