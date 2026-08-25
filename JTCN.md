# JTCN walkthrough

## 1. Node and sequencer core loop

| JTCN | Summary |
| ---: | --- |
| 15 | Node startup is complete. The Zone is loaded and L1 sync, P2P, RPC, and role management are running. |
| 19 | The leader's block production and L1 settlement workers are running. |
| 24 | Finalized L1 headers and Portal events have entered the engine queue. |
| 33 | One L1 input has produced a validated and saved Zone block. |
| 44 | The saved Zone blocks have been batched and submitted to the Portal on L1. |
| 54 | The submitted withdrawal queue has been drained through `processWithdrawals`. |

## 2. EVM, precompiles, and policy

| JTCN | Summary |
| ---: | --- |
| 62 | The Zone precompiles are installed and unsupported L1 services are removed. |
| 71 | `advanceTempo` has applied the finalized L1 checkpoint, Portal state, and deposits. |
| 77 | User transactions have run and the Outbox has committed withdrawals for settlement. |
| 85 | TIP 403 policy loading, caching, use during execution, and invalidation are covered. |

## 3. Portal contracts and cross-chain flows

| JTCN | Summary |
| ---: | --- |
| 89 | A deposit has passed Portal checks and entered the L1 deposit queue. |
| 98 | `submitBatch` has verified and committed the latest accepted Zone state on L1. |
| 102 | `processWithdrawals` has checked and advanced the Portal withdrawal queue. |
| 105 | The Portal state that acts as L1 source of truth is covered. |
| 114 | Portal roles, route permissions, pause controls, and exit protections are covered. |
| 123 | Deposit bouncebacks from the Zone to the chosen L1 refund address are covered. |
| 130 | Withdrawal bouncebacks from L1 to the chosen Zone recipient are covered. |

## 4. Private RPC, P2P, and leadership

| JTCN | Summary |
| ---: | --- |
| 138 | Redacted RPC authentication, allowed methods, scoped reads, and block redaction are covered. |
| 146 | The complete redacted HTTP and WebSocket RPC boundary is covered. |
| 155 | P2P peers, message channels, outbound checks, and inbound authentication are running. |
| 166 | A follower has received, reexecuted, validated, and saved a leader block. |
| 173 | Live replication, backfill, transaction forwarding, and settlement signatures are covered. |
| 181 | Leader changes, follower changes, fencing, and role handoff are covered. The walkthrough is complete. |
