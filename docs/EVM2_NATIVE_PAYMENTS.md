# EVM2 native Earn and Zones delivery contract

Status: implementation in progress. No acceptance gate below has passed yet.
This document records the requested scope and evidence required for completion;
it does not claim that specifications, existing PRs, or simulations deliver it.

## Completion rule

The goal is complete only when every requirement below has reproducible passing
evidence at the exact revisions in the final draft PR stack. A build, design,
single happy-path transaction, synthetic accounting model, or mocked proof is
insufficient. Keep the goal active while implementation, review fixes, migration,
or evidence is outstanding. Publish and attach each new draft PR; distinguish
existing upstream dependencies from deliverables produced by this work.

Every open item must have an implementation location, verification command,
expected assertion, and result artifact. A failure becomes a tracked finding;
fix it and rerun the affected composed flow before recording a pass. Never
substitute an easier workflow for an acceptance gate.

## Starting points

- Tempo EVM2: https://github.com/tempoxyz/tempo/pull/7871,
  initially inspected at `dfc750c5dbabee3b95d88393b71efaa1cca48584`.
- Zones EVM2: https://github.com/tempoxyz/zones/pull/1463. Its Tempo, Reth,
  Alloy, and EVM2 revisions must match the implementation stack.
- Native factory and proxy-compatible portal storage: TIP-1091 and
  `tempo/crates/precompiles/src/zone_factory/`.
- TIP-1084 vault-backed tokens: https://github.com/tempoxyz/tempo/pull/6559.
  The inspected draft excludes vault operations from payment classification.
  That exclusion requires a new bounded execution specification.
- Earn: `src/earn/VaultAdapter.sol`, `IVaultEngine`, ERC4626 and Veda engines,
  closed-loop ZoneGateway, contributions, fees, pending async claims, and
  deployment-fixed engine migration modes. Preserve their economic invariants.

These are dependencies, not evidence of this goal's completion.

## Protocol requirements

| ID | Required outcome | Passing evidence |
| --- | --- | --- |
| E01 | EVM2 is the only production execution engine for Tempo and Zones, including building, validation, RPC simulation/tracing, replay, and proving. | Dependency/features audit and builds of all shipped binaries; no selectable or fallback legacy executor. Shared legacy data types are documented separately from engine use. |
| E02 | EVM2 executes every historical Tempo and Zone fork from genesis with historical rules. | Differential replay against the current stack at every fork boundary, comparing state/receipt roots, logs, gas, errors, balances, and protocol system calls; a fresh peer syncs from genesis across activation. |
| E03 | New behavior is inactive before its named hardfork and deterministic at activation. | Parent, activation, and successor blocks; pool admission and validator agree; restart and reorg tests; no earlier block roots change. |
| C01 | Native precompiles call dependent contracts using EVM2 frames and the original transaction context. | Caller, origin, static propagation, depth, value, logs, tracing, nested native calls, storage warmness, and journal rollback assertions. No fabricated default transaction context. |
| C02 | Every child call shares a bounded paid parent budget. | Actual opcode/storage/proof work is charged on success, revert, and halt; cold-account and copy overhead, EIP-150 forwarding, execution/state gas, refunds, and child gas reconciliation tested. |
| C03 | Payment admission cannot expose unbounded arbitrary execution. | Exact selector/ABI allowlists and limits for calldata, returndata, proof bytes, batches, calls, gas, state growth, and depth; malicious dependencies/proxies cannot create free work. Identical pool/builder/validator decisions. |
| C04 | Parent and child failure semantics are atomic and specified. | Reentrancy, nested failure, OOG, DB failure, static mutation, forged callback, partial batch, oversized return, and replay tests; balances, supply, queue cursors, locks, and logs match the specified rollback boundary. |
| Z01 | Native portals implement deposit, refund, withdrawal, callback, and settlement with existing identity/storage continuity. | Existing and new Zones preserve addresses, configuration, roles, encryption keys, queues, commitments, deposits, pending withdrawals, and balances. Differential Solidity/native transition tests. |
| Z02 | Real private transfers and withdrawals produce verifiable commitments/proofs and settle on Tempo. | Sequencer and prover run the same fork schedule; genuine proof accepted by verifier; invalid, stale, duplicate, wrong-chain, and wrong-height proofs rejected; forced exit and refund paths tested. |
| Z03 | Existing authorization, privacy, token policy, and custody protections apply to native portals. | Sequencer threshold, leader/version, TIP-403 sender/recipient/mint, access/gateway rules, pause/abdication, protected custody, and encrypted return tests. No private identifiers in public logs. |
| A01 | Native Earn supports vault deposit, proportional share issuance, transfers, yield accounting, contribution, redemption, exact-asset withdrawal, and spending. | Real venue-backed balances and supply; positive yield, loss, zero supply, rounding, high-water fees, contribution without funder shares, slippage, and delegated spending assertions. |
| A02 | Earn works with both synchronous and existing asynchronous engine capabilities. | Request burns shares once, pending claims excluded from active NAV, immutable stored receiver/queue, finalize/cancel, stale report exit, pause, engine migration restrictions, and unresolved-claim tests. |
| A03 | Combined Earn/Zone flows execute in the payment lane end to end. | Public and private entry, private share transfer, private yield, redemption, spend from Earn, return to originating Zone, partial redemption, and failed callback bounce; every payment receipt and settlement has zero attributed general-lane gas. |
| M01 | Existing public balances, private balances, vault backing/positions, fee claims, and pending async claims migrate without duplication or loss. | Fund pre-upgrade state with real transactions, activate, reconcile balances/supply/backing/claims and queue commitments, then redeem/transfer/settle old positions. Retry/restart/reorg cannot remigrate. |
| M02 | Migration work is bounded, deterministic, and charged or reserved explicitly. | No unbounded scan at activation; paged/lazy mechanism with stable commitment and progress, adversarial state-size tests, and block budget accounting. No hidden mint authority or custodial reassignment. |
| I01 | SDKs, RPC, ABIs, indexers, deployment manifests, prover inputs, images, and infra support both sides of the fork. | Versioned compatibility matrix, generated ABI checks, client integration tests, reproducible deployment, exact revision/PCR manifests, and documented recovery/rollback limitations. |

## Execution design constraints

Native contract calls use the same EVM state journal, transaction environment,
gas tables, state reservoir, and inspector as ordinary execution. Native handlers
must release scoped thread-local storage borrows before entering a child frame;
recursive native execution must not alias mutable storage or lose action replay.
The adapter cannot spend `u64::MAX` gas or call a new independent EVM instance.

Admitting a native address alone is insufficient. Only the specified payment
selectors with bounded inputs qualify. Administrative deployment/configuration
transactions may use the general lane and must be reported separately. Every
required deposit, transfer, exit, settlement, contribution, and Earn-spend path
must qualify without falling back to a general transaction. Arbitrary user
targets may execute only under the shared capped paid callback budget; nested
calls cannot reset it. Registration is not a claim of venue solvency.

Do not edit historical classification or gas schedules to make new tests pass.
Use explicit fork gating and replay existing proxy runtime before activation.
Only factory-registered canonical portals are eligible for native dispatch;
prefix collisions and uninitialized accounts must not bypass validation.

Native vault accounting must conserve claims on backing. Non-rebasing Earn
balances change through issuance/burn/transfer only; yield or losses change NAV.
Checkpoint fees before entry or migration. Contributions cannot dilute holders
or mint funder shares. Pending async exits and settled-but-unclaimed assets
remain explicit liabilities, with their original authenticated receiver.

## Delivery sequence

1. Pin compatible upstream EVM2/Tempo/Reth/Zones revisions. Establish the
   current-stack baseline and record known historical mismatches.
2. Draft TIPs for native payment execution/resource metering, portals, Earn,
   migration and activation; reference or amend existing TIPs where appropriate.
   Reserve TIP numbers using repository process. Drafts do not assign a real
   network activation or claim governance approval.
3. Implement and test EVM2 frame/gas/context plumbing and historical replay.
   Remove legacy production engine only after the replay gate passes.
4. Implement native portals and vaults behind the explicit new fork; align
   transaction admission, block accounting, contracts, SDKs, and proving.
5. Implement state migration and deployment manifests; compose all flows on
   a local devnet with real Tempo and Zone nodes, venues, and proof settlement.
6. Exercise scheduled activation with live old positions and an existing Zone;
   run a fresh genesis sync, reorg/restart, combined load, and failure campaigns.
7. Review security and correctness, fix each finding, refresh evidence at the
   final revisions, and publish all draft implementation PRs with demonstrations.

## Required devnet and benchmark artifact

Check in a command in the Tempo or Zones implementation PR that provisions a
disposable devnet and returns nonzero on any invariant or lane assertion failure.
It must execute the actual scheduled upgrade, rather than enabling all forks at
genesis. Record baseline and upgraded runs with the same workload and hardware.

The artifact must contain:

- exact source revisions, lockfiles, binaries/images, genesis hashes, fork
  timestamps/heights, proof/verifier versions, and machine/resource limits;
- pre-fork funded positions and queue state, post-fork reconciliation, receipt
  hashes and block numbers, accepted proof and settlement hashes;
- balances, total supply, backing, fees, pending/settled claims, queue cursors,
  and rounding/dust reconciliation before and after each combined flow;
- per-operation gas and execution/state work, payment/general attribution from
  execution and block counters, including callbacks and settlement;
- attempted/submitted/included/successful/settled operations, queue growth,
  p50/p95/p99 latency, CPU, memory, disk/state growth, proof lag and failure rate;
- at least 30 minutes of mixed Earn/Zone steady-state load after warmup, with
  multiple users/vaults/Zones, rewards, transfers, redemption, spending, and
  settlement running concurrently, followed by complete queue drain;
- a general-lane saturation run demonstrating continued payment progress and
  zero general-lane usage attributable to the required payment flows;
- deterministic fixtures for hostile callbacks, bad proofs, replay, OOG,
  slippage, paused token/venue, stale valuation, unavailable venue, and reorg.

Thirty minutes is an initial minimum test duration, not an invented TPS promise.
Report measured capacity and comparison with baseline; investigate regressions.
Zero lane usage must be measured, not inferred from transaction destinations.
The demo must verify proofs using the intended cryptographic backend; local
test-proof bypasses cannot satisfy Z02 or the completion gate.

## Review and handoff

Maintain a finding ledger with severity, attack/failure trigger, affected
invariant, reproduction, fix revision, and passing regression. Do not label a
self-review as an independent audit. External review/approval unavailable to
the agent must be reported honestly; no fabricated reviewer or signoff.

Final handoff includes links to every new draft TIP and implementation PR,
dependency ordering, exact devnet command, evidence artifacts, benchmark
comparison, migration reconciliation, historical replay/sync results, and
resolved review findings. Any unmet row keeps this delivery contract incomplete.

## Current work ledger

| Work | State | Evidence |
| --- | --- | --- |
| Repository/upstream inventory | Started | Tempo #7871, Zones #1463, TIP-1084 #6559; local Earn invariants inspected |
| Compatible EVM2 baseline | In progress | Tempo worktree on `dan/evm2-native-payments`; initial compilation running |
| E01–I01 | Open | No passing result recorded |
| Draft TIPs / implementation PRs | Open | No new PR produced yet |
