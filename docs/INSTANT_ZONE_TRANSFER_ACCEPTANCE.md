# Instant Zone transfer acceptance status

This draft is not the completed instant-transfer protocol. Fast execution stays disabled and no
ten-Zone payment, replenishment, chaos, or throughput result is claimed. The source specification
is attached to the [implementation request](https://tempoxyz.enterprise.slack.com/archives/C0A87C21805/p1791158720260559).
Its latency figures are measurement objectives, not hard acceptance cutoffs for this request.

## Required protocol decision

The specification requires a coordinated opt-in protocol upgrade but does not assign a Tempo
hardfork/TIP or pin a functioning proof-required verifier configuration. Reusing T13 changes
historical consensus; adding the behavior to the existing T14 upgrade also requires an explicit
protocol-owner decision. Neither has been inferred here.

The pinned Tempo revision has no finalized fast-epoch factory dispatch, matching Portal runtime,
same-anchor capability, or new proof format. The native factory installs ERC-1167 Portal proxies;
Portal calls execute the shared Solidity runtime, not a native Portal precompile. Runtime bytes,
native factory storage initialization, genesis installation, hardfork replacement, and Rust ABI
bindings must change together in the matching Tempo prerequisite.

## Implementation boundaries

| Requirement | Draft work | Remaining acceptance prerequisite |
| --- | --- | --- |
| C1: fork and same-anchor execution | Encoded opening, payload/executor/SPF representation and fail-closed gates | Selected fork, anchored epoch import, hardfork-policy validation, transaction-arrival scheduler and live clock admission |
| C2: durable quorum commitment | Maintained OpenRaft adapter and committed-outcome/replay/signing interfaces | Persistent Raft storage, authenticated replica network, node assembly, recovery, canonical-head promotion and fault tests |
| C3: native escrow and pools | Dormant ABI/state transitions and existing-token accounting | Unified EVM/transport identity and encoding, finalized registry/asset/quote verification, atomic token/policy acceptance and real execution tests |
| C4: direct operator protocol | Canonical peer messages and fsynced service journal | Mutually authenticated streams, replica endpoint reconnection, committed-state worker reconstruction and private RPC/subscriptions |
| C5: recovery and admission | Count/value/byte budgets, replay markers and monotonic states | Actual committed recovery, reserved terminal throughput, policy-blocked liabilities and all nine authenticated peer barriers |
| C6: replenishment | Permanent job identities, amount/nonce reconciliation records and stage transitions | Actual source allocation/withdrawal, treasury reconciliation, encrypted deposit, canonical bounce/refund evidence and fee accounting |
| C7: ten-Zone proof of behavior | Explicit failing real-factory infrastructure gate | Ten distinct Zones with three durable replicas each, real funding, A→B→C/local spend, accepted-proof settlement and full fault/load suite |

The preliminary Portal registry ABI is dormant. Before activation it must bind the exact enrolled
peer identities, use explicit duplicate markers, authenticate all lock-watermark/unresolved roots,
prove resolution and final settlement, and require installation of the next roster's checkpoint.
It must retain historical keys and prevent all legacy authority paths from replacing the committed
prefix. A count of arbitrary peer barriers cannot prove retirement is safe.

The transport and native ABI currently remain separate representations. Transport-only encoding
vectors are not cross-language EVM conformance evidence; activation must wait for one shared
transfer-ID/intent/certificate contract and literal vectors tested against both representations.

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

## Validation evidence

The initial bare-metal snapshot passed all 28 `zone-fast-transfer` tests: 12 source-local units,
2 independent model tests, 5 certificate tests, 7 protocol boundary tests and 2 journal acceptance
tests. Strict nightly Clippy then found an oversized journal enum variant; boxing that variant
preserves its on-disk encoding. A separate fresh validation agent reruns after corrections.

The fresh independent reviewer subsequently reran all 30 current fast-transfer tests successfully
(14 source-local and 16 acceptance tests) and passed the workspace formatting check. It rejected
full E2E acceptance, confirming dormant execution, divergent native/transport bindings, absent
replica/worker assembly and missing actual spend/replenishment/chaos coverage. Strict Clippy found
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

There is no accepted ten-Zone E2E result. Do not enable the compatibility pin or remove the
activation gates based on these unit, model, journal or Solidity results.
