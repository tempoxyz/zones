# JTCN walkthrough

This is the table of contents for the Zone walkthrough. In Neovim, type the JTCN number followed by `]j` to jump directly to it. For example, `54]j` jumps to JTCN 54.

Use `]j` and `[j` to move forward and backward one note. Use `:Jtcn 0` to start from the beginning.

## Today's one hour tour

This route follows one Zone block from startup through L1 ingestion, execution, policy checks, and settlement on the Portal. The timings are intentionally tight. Stop at each checkpoint for questions, then keep moving.

| Time | Read | What to focus on |
| ---: | :--- | --- |
| 4 min | JTCN 0 to 15 | Skim startup. Identify the L1 subscriber, P2P, RPC, role controller, Zone engine, and settlement workers. |
| 6 min | JTCN 16 to 24 | See how becoming leader starts `ZoneEngine`, then follow finalized L1 headers and Portal events into `DepositQueue`. |
| 7 min | JTCN 25 to 33 | Follow one queued L1 block through payload building, validation, and the Zone DB. |
| 3 min | JTCN 34, 44, 45, and 54 | Take the short settlement path. See the batch monitor submit saved blocks and the withdrawal worker drain accepted queues. |
| 6 min | JTCN 55 to 62 | Build the Zone EVM and get a map of every Zone precompile and what it owns. |
| 12 min | JTCN 63 to 71 | Go deep on `advanceTempo`. Follow the system transaction, L1 and Portal checks, deposit decryption, minting, bounceback creation, and committed state. |
| 10 min | JTCN 72 to 85 | Run user transactions, commit withdrawals through the Outbox, then follow TIP 403 reads through the L1 cache and Zone DB adapter. |
| 12 min | JTCN 86 to 105 | Follow the Portal contract through deposits, `submitBatch`, `processWithdrawals`, and the L1 state that remains the source of truth. |

For the short settlement path, jump with `34]j`, `44]j`, `45]j`, and `54]j`. The first hour ends at checkpoint 105 with the complete core execution and settlement story.

## Optional second hour

| Time | Read | What it adds |
| ---: | :--- | --- |
| 8 min | JTCN 106 to 114 | Portal roles, route permissions, pause controls, and exit protections. |
| 12 min | JTCN 115 to 130 | Throughput caps and both complete bounceback paths. |
| 15 min | JTCN 131 to 146 | Authentication and the redacted HTTP and WebSocket RPC boundary. |
| 25 min | JTCN 147 to 181 | P2P channels, follower replication, backfill, settlement signatures, and leader failover. |

## 1. Node and sequencer core loop

| JTCN | Jump | Checkpoint |
| ---: | :---: | --- |
| 15 | `15]j` | Node startup is complete. The Zone is loaded and L1 sync, P2P, RPC, and role management are running. |
| 19 | `19]j` | The leader's block production and L1 settlement workers are running. |
| 24 | `24]j` | Finalized L1 headers and Portal events have entered the engine queue. |
| 33 | `33]j` | One L1 input has produced a validated and saved Zone block. |
| 44 | `44]j` | The saved Zone blocks have been batched and submitted to the Portal on L1. |
| 54 | `54]j` | The submitted withdrawal queue has been drained through `processWithdrawals`. |

## 2. EVM, precompiles, and policy

| JTCN | Jump | Checkpoint |
| ---: | :---: | --- |
| 62 | `62]j` | The Zone precompiles are installed and unsupported L1 services are removed. |
| 71 | `71]j` | `advanceTempo` has applied the finalized L1 checkpoint, Portal state, and deposits. |
| 77 | `77]j` | User transactions have run and the Outbox has committed withdrawals for settlement. |
| 85 | `85]j` | TIP 403 policy loading, caching, use during execution, and invalidation are covered. |

## 3. Portal contracts and cross-chain flows

| JTCN | Jump | Checkpoint |
| ---: | :---: | --- |
| 89 | `89]j` | A deposit has passed Portal checks and entered the L1 deposit queue. |
| 98 | `98]j` | `submitBatch` has verified and committed the latest accepted Zone state on L1. |
| 102 | `102]j` | `processWithdrawals` has checked and advanced the Portal withdrawal queue. |
| 105 | `105]j` | The Portal state that acts as L1 source of truth is covered. |
| 114 | `114]j` | Portal roles, route permissions, pause controls, and exit protections are covered. |
| 123 | `123]j` | Deposit bouncebacks from the Zone to the chosen L1 refund address are covered. |
| 130 | `130]j` | Withdrawal bouncebacks from L1 to the chosen Zone recipient are covered. |

## 4. Private RPC, P2P, and leadership

| JTCN | Jump | Checkpoint |
| ---: | :---: | --- |
| 138 | `138]j` | Redacted RPC authentication, allowed methods, scoped reads, and block redaction are covered. |
| 146 | `146]j` | The complete redacted HTTP and WebSocket RPC boundary is covered. |
| 155 | `155]j` | P2P peers, message channels, outbound checks, and inbound authentication are running. |
| 166 | `166]j` | A follower has received, reexecuted, validated, and saved a leader block. |
| 173 | `173]j` | Live replication, backfill, transaction forwarding, and settlement signatures are covered. |
| 181 | `181]j` | Leader changes, follower changes, fencing, and role handoff are covered. The walkthrough is complete. |
