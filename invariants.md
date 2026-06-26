# Zone Invariants

This document lists the core protocol invariants for Tempo Zones. It is intended
for auditors, invariant/fuzz test authors, and production monitoring.

Invariant IDs follow the `TEMPO-<MODULE>-<NAME>` pattern used by Tempo Monitor.

## Criticality

| Criticality | Meaning |
|-------------|---------|
| 🔴 **CRIT** | Direct loss of funds, invalid state transition, or unrecoverable queue corruption if violated |
| 🟡 **HIGH** | Governance/access-control breakage, privacy breakage, fund lock, replay, or proof soundness issue |
| 🟢 **MED** | Structural inconsistency, operational DoS, accounting drift, or monitoring-relevant degradation |

## Tiers

| Tier | When it should run |
|------|--------------------|
| **Tip-critical** | Every block or every relevant state transition |
| **Audit-only** | Full-state scan, fuzz target, proof harness, or offline audit |

## Invariants

### Zone Registry and Deployment

| ID | Assertion | Tier | Crit | Impact | Recovery |
|---|---|---|---|---|---|
| `TEMPO-ZONE-CHAIN-ID-UNIQUE` | Each live zone uses the chain ID derived from its zone ID, and no two live zones share a chain ID | Audit-only | 🟡 | Cross-zone replay protection fails; signed transactions may be valid on more than one zone | Stop accepting traffic on the duplicate zone, redeploy or migrate to a unique chain ID, and invalidate affected client configs |
| `TEMPO-ZONE-PORTAL-PAIRING` | A `ZoneFactory` registry entry maps one zone ID to exactly one portal and messenger pair | Audit-only | 🟡 | Deposits, withdrawals, callbacks, and config reads can target different trust domains | Halt the zone, compare factory events against portal state, and redeploy or pin clients to the canonical portal |
| `TEMPO-ZONE-GENESIS-BINDING` | Portal `blockHash`, `genesisTempoBlockNumber`, and emitted zone creation parameters match the zone genesis file | Audit-only | 🔴 | The zone may prove batches from a different genesis state than the portal expects | Refuse startup, regenerate genesis from canonical factory events, and redeploy before user deposits |
| `TEMPO-ZONE-PREDEPLOY-ADDRESSES` | `TempoState`, `ZoneInbox`, `ZoneOutbox`, `ZoneConfig`, `TempoStateReader`, and `ZoneTxContext` exist at their fixed addresses | Audit-only | 🔴 | System calls can be redirected or missing, invalidating mint/burn, proofs, and Tempo reads | Halt block production and restart from a genesis with canonical predeploy state |

### Access Control and Configuration

| ID | Assertion | Tier | Crit | Impact | Recovery |
|---|---|---|---|---|---|
| `TEMPO-ZONE-ADMIN-NONZERO` | Portal `admin != address(0)` for every zone | Audit-only | 🟡 | Token governance can become permanently unavailable | Treat the zone as misconfigured; deploy a replacement portal before enabling material deposits |
| `TEMPO-ZONE-ADMIN-ONLY-GOVERNANCE` | Only `admin` can call `enableToken`, `pauseDeposits`, and `resumeDeposits` | Tip-critical | 🟡 | A sequencer or user can enable malicious assets or reopen paused deposits | Pause operations, rotate admin if compromised, and replay governance events to identify unauthorized changes |
| `TEMPO-ZONE-SEQUENCER-ONLY-OPS` | Only the registered sequencer can set gas rates, set encryption keys, set RPC URL, submit batches, and process withdrawals | Tip-critical | 🟡 | Unauthorized operators can censor, misprice, settle, or drain queued work | Stop the zone, rotate sequencer through the portal, and invalidate unauthorized batches or pending operations |
| `TEMPO-ZONE-SEQUENCER-TWO-STEP` | Sequencer changes only complete when `pendingSequencer` accepts, and acceptance clears `pendingSequencer` | Tip-critical | 🟡 | Sequencer control can be accidentally or maliciously transferred | Freeze operations, inspect `SequencerTransferStarted` / `SequencerTransferred`, and rotate to the intended sequencer |
| `TEMPO-ZONE-GAS-RATE-BOUNDED` | `zoneGasRate` and `tempoGasRate` never exceed `MAX_GAS_FEE_RATE` | Tip-critical | 🟢 | Deposit or withdrawal fee math may overflow or become economically unusable | Revert the rate update, restore the prior rate, and alert on the attempted over-limit configuration |

### Token Registry and Supply

| ID | Assertion | Tier | Crit | Impact | Recovery |
|---|---|---|---|---|---|
| `TEMPO-ZONE-TOKEN-ENABLEMENT-APPEND-ONLY` | Once enabled, a token remains enabled and remains in the append-only enabled token list | Tip-critical | 🔴 | Withdrawals can be disabled after deposits, breaking the non-custodial bridge guarantee | Halt deposits for the affected token, restore registry state from portal events, and reconcile zone token deployment |
| `TEMPO-ZONE-TOKEN-DEPOSIT-PAUSE-ONLY` | Pausing a token only disables new deposits; withdrawals for enabled tokens remain requestable and processable | Tip-critical | 🔴 | Admin can lock users inside the zone by pausing deposits | Treat as a critical bridge violation, unblock withdrawals, and reconcile affected user balances |
| `TEMPO-ZONE-MESSENGER-APPROVAL` | For every enabled token, the portal approves the zone messenger for callback withdrawals | Audit-only | 🟡 | Callback withdrawals can fail even when the portal holds enough funds | Re-approve the messenger for affected tokens and reprocess failed withdrawals through bounce-back flow if needed |
| `TEMPO-ZONE-SUPPLY-SOLVENCY` | For each token, zone-side total supply equals accepted deposits plus withdrawal bounce-backs minus requested withdrawals minus deposit bounce-backs | Audit-only | 🔴 | The zone can mint unbacked tokens or burn user funds without matching L1 release | Halt settlement, reconcile deposit and withdrawal event chains, and roll forward only from a proven consistent state |
| `TEMPO-ZONE-PORTAL-SOLVENCY` | Portal token balance plus paid-out/parked refunds is sufficient for all unwithdrawn zone supply and pending withdrawals | Audit-only | 🔴 | Portal cannot honor exits, causing direct loss or insolvency | Stop new deposits, process or park outstanding refunds, and reconcile against token transfer logs before resuming |
| `TEMPO-ZONE-MINT-BURN-AUTHORITY` | Only `ZoneInbox` can mint zone tokens and only `ZoneOutbox` can burn zone tokens | Tip-critical | 🔴 | Unauthorized mint or burn breaks bridge accounting and can steal or destroy funds | Halt the zone, identify the unauthorized call path, and restore from the last valid state transition |

### Deposits

| ID | Assertion | Tier | Crit | Impact | Recovery |
|---|---|---|---|---|---|
| `TEMPO-ZONE-DEPOSIT-ENABLED-ACTIVE` | User deposits only enter the queue when the token is enabled and deposits are active | Tip-critical | 🟡 | Users can deposit unsupported or paused assets that the zone may not process | Reject or refund invalid deposits and compare queue entries against token config at enqueue time |
| `TEMPO-ZONE-DEPOSIT-FEE-SNAPSHOT` | Deposit queue entries store `amount - FIXED_DEPOSIT_GAS * zoneGasRate`, and the fee is paid to the sequencer at enqueue time | Tip-critical | 🟢 | Fee changes can retroactively change user value or underpay processing costs | Reconcile `DepositMade` events against token transfers and restore correct net amounts before processing |
| `TEMPO-ZONE-DEPOSIT-BOUNCEBACK-NONZERO` | Every user-initiated deposit has a non-zero, TIP-403-authorized `bouncebackRecipient` | Tip-critical | 🔴 | Failed deposits can permanently block or strand funds without a refund target | Reject the deposit before queue insertion; if already queued, force a safe refund path before advancing |
| `TEMPO-ZONE-DEPOSIT-QUEUE-HASH` | Portal deposit queue hash updates as `keccak256(abi.encode(depositType, depositData, previousHash))` for every regular or encrypted deposit | Tip-critical | 🔴 | The zone may process a different deposit sequence than the portal accepted | Halt `advanceTempo`, recompute from events, and resume only when portal and inbox hashes agree |
| `TEMPO-ZONE-DEPOSIT-NUMBER-MONOTONIC` | `depositCount` and `processedDepositNumber` are monotonic and match the number of queue entries enqueued or proven processed | Tip-critical | 🟢 | User deposit status can be wrong and deposits may be skipped or double-counted | Recompute counters from queue events and reject batches with inconsistent transitions |
| `TEMPO-ZONE-DEPOSIT-PROCESSED-PREFIX` | The inbox processes only a prefix of the portal queue, oldest first, and never skips, reorders, or duplicates deposits | Tip-critical | 🔴 | Users receive wrong mints/refunds or deposits become unprovable | Reject the batch proof, recompute the expected queue prefix, and restart from the prior processed hash |
| `TEMPO-ZONE-DEPOSIT-FAIL-BOUNCEBACK` | Any failed regular mint, rejected deposit, invalid encrypted deposit, or failed encrypted mint enqueues exactly one deposit bounce-back withdrawal | Tip-critical | 🔴 | Failed deposits can be lost, duplicated, or stuck | Process the bounce-back queue, park failed refunds in the refund registry, and reconcile failed deposit events |
| `TEMPO-ZONE-DEPOSIT-REJECTION-NO-MINT` | A rejected user deposit never mints zone tokens and still advances the deposit queue | Tip-critical | 🔴 | Sequencer rejection can create unbacked mints or stall deposits | Reject the batch and verify `DepositRejected` events have matching bounce-back withdrawals |

### Encrypted Deposits and Keys

| ID | Assertion | Tier | Crit | Impact | Recovery |
|---|---|---|---|---|---|
| `TEMPO-ZONE-ENCRYPTION-KEY-APPEND-ONLY` | Sequencer encryption keys are appended with valid secp256k1 points and proof of possession; historical entries never mutate | Tip-critical | 🟡 | Sequencer can register unusable keys or rewrite history, causing undecryptable deposits | Reject invalid key updates, rotate to a valid key, and bounce back deposits encrypted to invalid entries |
| `TEMPO-ZONE-ENCRYPTION-KEY-GRACE` | Non-current encryption keys are accepted only until the next key activation block plus `ENCRYPTION_KEY_GRACE_PERIOD`; the current key does not expire | Tip-critical | 🟢 | Users can enqueue deposits to expired keys or have current-key deposits rejected | Reject invalid deposits at enqueue time and update clients to the latest valid key index |
| `TEMPO-ZONE-ENCRYPTED-PAYLOAD-SHAPE` | Encrypted deposits require valid ephemeral public key parity/X coordinate and exactly 64 bytes of ciphertext | Tip-critical | 🟢 | Oversized or invalid payloads can DoS zone-side decryption or make proofs impossible | Reject before queue insertion and alert on malformed encrypted-deposit attempts |
| `TEMPO-ZONE-DECRYPTION-ORDER` | Decryption data is consumed one-for-one, in order, for accepted encrypted deposits only | Tip-critical | 🔴 | A sequencer can apply a proof to the wrong ciphertext or desynchronize processing | Reject the batch, replay the deposit queue, and verify decryption index accounting |
| `TEMPO-ZONE-CHAUM-PEDERSEN-BINDING` | Accepted encrypted deposits only decrypt using a valid Chaum-Pedersen proof tied to the stored sequencer key for `keyIndex` | Tip-critical | 🔴 | Sequencer can substitute keys or fabricate plaintext, redirecting deposits | Reject the proof, bounce back the deposit, and rotate keys if the sequencer key is compromised |
| `TEMPO-ZONE-AES-GCM-AUTHENTICITY` | If AES-GCM authentication or plaintext length validation fails, no mint is attempted and the deposit bounces back | Tip-critical | 🔴 | Invalid ciphertext can mint to attacker-chosen or malformed recipients | Reject the batch and confirm `EncryptedDepositFailed` plus matching bounce-back output |

### Withdrawals

| ID | Assertion | Tier | Crit | Impact | Recovery |
|---|---|---|---|---|---|
| `TEMPO-ZONE-WITHDRAWAL-TOKEN-ENABLED` | Withdrawals can only be requested for enabled tokens | Tip-critical | 🔴 | Users can burn unsupported assets with no corresponding portal escrow | Reject the request and monitor for disabled-token withdrawal attempts |
| `TEMPO-ZONE-WITHDRAWAL-FALLBACK-NONZERO` | Every user withdrawal has a non-zero `fallbackRecipient` | Tip-critical | 🔴 | Failed Tempo-side withdrawals cannot return funds to the zone | Reject the request; if legacy data exists, process as failed and park funds under an explicit owner |
| `TEMPO-ZONE-WITHDRAWAL-FEE-SNAPSHOT` | Withdrawal fee equals `(WITHDRAWAL_BASE_GAS + gasLimit) * tempoGasRate` at request time and is burned with the amount | Tip-critical | 🟢 | Fee changes retroactively alter user economics or underfund processing | Recompute from `WithdrawalRequested`, reject mismatches, and restore the correct rate for future requests |
| `TEMPO-ZONE-WITHDRAWAL-BURN-BEFORE-QUEUE` | `requestWithdrawal` burns `amount + fee` before appending the pending withdrawal | Tip-critical | 🔴 | Portal can release funds without removing zone supply | Reject the request path and reconcile token supply against withdrawal events |
| `TEMPO-ZONE-WITHDRAWAL-CALLBACK-BOUNDS` | `gasLimit <= MAX_WITHDRAWAL_GAS_LIMIT`, callback data is bounded, and over-limit legacy withdrawals bounce back after dequeue | Tip-critical | 🟡 | A withdrawal can exceed block gas limits or permanently block the FIFO queue | Dequeue and bounce back the offending withdrawal, then tighten request-time validation |
| `TEMPO-ZONE-SENDER-TAG-BINDING` | `senderTag == keccak256(abi.encodePacked(sender, txHash))`, where `txHash` is the current withdrawal request transaction hash | Tip-critical | 🟡 | Authenticated withdrawals can reveal or misattribute the sender | Reject the batch and verify `ZoneTxContext.currentTxHash()` is non-zero and transaction-specific |
| `TEMPO-ZONE-ENCRYPTED-SENDER-SHAPE` | If `revealTo` is set, `encryptedSender` is present and exactly 113 bytes; otherwise it is empty | Tip-critical | 🟢 | Selective reveal consumers cannot authenticate sender metadata reliably | Reject finalization with malformed encrypted sender data and regenerate the withdrawal batch |
| `TEMPO-ZONE-WITHDRAWAL-BATCH-INDEX` | `finalizeWithdrawalBatch` advances `withdrawalBatchIndex` exactly once per submitted batch, including zero-withdrawal batches | Tip-critical | 🔴 | Sequencer can omit or replay batches containing withdrawals | Reject `submitBatch`, compare expected index to outbox last batch, and resubmit missing batches in order |
| `TEMPO-ZONE-WITHDRAWAL-HASH-LIFO-FIFO` | Outbox builds each withdrawal hash chain LIFO so the portal processes user withdrawals FIFO | Tip-critical | 🔴 | Withdrawal order can be reversed, skipped, or duplicated | Reject the batch, reconstruct from pending withdrawal events, and resubmit with the canonical hash chain |
| `TEMPO-ZONE-WITHDRAWAL-QUEUE-RING` | Portal withdrawal queue satisfies `tail >= head`, `tail - head <= WITHDRAWAL_QUEUE_CAPACITY`, and empty slots equal `EMPTY_SENTINEL` | Tip-critical | 🔴 | Queue overflow or stale slot reuse can lose or replay withdrawals | Stop batch submission, drain processable slots, and repair by replaying accepted `BatchSubmitted` events |
| `TEMPO-ZONE-WITHDRAWAL-DEQUEUE-AUTH` | `processWithdrawal` only dequeues when `keccak256(abi.encode(withdrawal, remainingQueue))` matches the current head slot | Tip-critical | 🔴 | Sequencer can process arbitrary withdrawals or steal portal escrow | Reject the call, alert immediately, and verify the queue head against the submitted calldata |
| `TEMPO-ZONE-WITHDRAWAL-POP-ONCE` | Each processed withdrawal is popped exactly once, whether transfer/callback succeeds or bounces back | Tip-critical | 🔴 | Failed withdrawals can block the queue or successful withdrawals can be replayed | Recompute the head slot after each processing event and stop on any duplicate queue hash |
| `TEMPO-ZONE-WITHDRAWAL-FAIL-BOUNCEBACK` | Any failed user-facing transfer or callback enqueues exactly one withdrawal bounce-back deposit for `amount`, excluding fee | Tip-critical | 🔴 | Failed withdrawals can lose funds or duplicate refunds | Reconcile `WithdrawalProcessed(success=false)` with deposit queue changes and process the bounce-back path |
| `TEMPO-ZONE-DEPOSIT-BOUNCEBACK-FEE-CAP` | Deposit bounce-back fee is computed at processing time and capped at the bounced amount | Tip-critical | 🟢 | Refund accounting can underflow or overpay the sequencer | Reject the processing result and park the correct net refund in the portal refund registry |

### Batch Submission and Proofs

| ID | Assertion | Tier | Crit | Impact | Recovery |
|---|---|---|---|---|---|
| `TEMPO-ZONE-BATCH-PREV-HASH` | Submitted `blockTransition.prevBlockHash` equals the portal's current `blockHash` | Tip-critical | 🔴 | A batch can fork from an uncommitted zone state | Reject `submitBatch` and resubmit from the current portal block hash |
| `TEMPO-ZONE-BATCH-NEXT-HASH` | Accepted proof output commits to the full next zone block hash, including state, transactions, receipts, number, timestamp, beneficiary, and protocol version | Tip-critical | 🔴 | Proof can validate a different state transition than the portal records | Reject verifier output that is not bound to the complete header hash |
| `TEMPO-ZONE-BATCH-DEPOSIT-TRANSITION` | Deposit transition starts from the inbox's previous processed hash/number and ends at the post-batch processed hash/number | Tip-critical | 🔴 | Deposits can be skipped, replayed, or falsely marked processed | Reject the batch and recompute the transition from executed `advanceTempo` calls |
| `TEMPO-ZONE-BATCH-WITHDRAWAL-COMMITMENT` | Submitted `withdrawalQueueHash` equals `ZoneOutbox.lastBatch.withdrawalQueueHash` from the proven post-state | Tip-critical | 🔴 | Portal can enqueue withdrawals that the zone never finalized | Reject the batch and compare the submitted queue hash against outbox storage in the proof |
| `TEMPO-ZONE-BATCH-ANCHOR-BLOCK` | Anchor block number/hash passed to the verifier matches either the direct Tempo binding or a valid ancestry chain to a recent Tempo block | Tip-critical | 🔴 | Proof can rely on a stale or forged Tempo view | Reject the batch and resubmit with a valid finalized Tempo anchor |
| `TEMPO-ZONE-BATCH-SEQUENCER-BENEFICIARY` | Every proven zone block has `beneficiary == portal.sequencer` | Tip-critical | 🟡 | A non-sequencer can produce blocks or collect block-level authority | Reject the proof and rotate sequencer if the portal state was compromised |
| `TEMPO-ZONE-BATCH-FINALIZE-LAST` | Intermediate blocks do not finalize withdrawals; the final block executes `finalizeWithdrawalBatch` last | Tip-critical | 🔴 | Withdrawals can be omitted from the committed state or finalized before later user transactions | Reject the witness and enforce block transaction ordering in the prover |
| `TEMPO-ZONE-PROOF-MISSING-READS` | Any zone-state or Tempo-state read missing from the witness causes proof failure; missing reads never default silently | Tip-critical | 🔴 | Prover can omit non-zero state and prove an invalid transition | Reject the proof and add witness coverage checks for every account and storage access |

### Tempo State Reads and TIP-403

| ID | Assertion | Tier | Crit | Impact | Recovery |
|---|---|---|---|---|---|
| `TEMPO-ZONE-TEMPO-HEADER-CONTINUITY` | `TempoState.finalizeTempo` only accepts headers whose parent hash and block number continue from the previous finalized Tempo header | Tip-critical | 🔴 | Zone reads can bind to a forged or discontinuous Tempo history | Reject `advanceTempo`, restart from the last valid finalized header, and resync from Tempo finality |
| `TEMPO-ZONE-TEMPO-READ-AUTHZ` | Only zone system contracts can read arbitrary Tempo storage through `TempoState.readTempoStorageSlot` | Tip-critical | 🟡 | Users can inspect L1-derived private or policy state through system read paths | Reject user calls and audit all precompile call sites for system-only gating |
| `TEMPO-ZONE-TEMPO-READ-ROOT` | Every Tempo storage read is proven against the `tempoStateRoot` bound at the block where the read occurs | Tip-critical | 🔴 | Configuration, token, policy, or queue reads can be forged | Reject the proof and recompute reads against the finalized header root |
| `TEMPO-ZONE-TIP403-INHERITANCE` | Zone token transfer, mint, and withdrawal paths enforce the TIP-403 policy inherited from the current finalized Tempo view | Tip-critical | 🔴 | Blacklisted or unauthorized accounts can move, receive, mint, or withdraw funds | Halt affected token flows, import the latest finalized Tempo policy state, and replay failed transfers |
| `TEMPO-ZONE-TIP403-READONLY` | Zone-side TIP-403 registry/proxy cannot mutate policy state | Tip-critical | 🟡 | A zone user or sequencer can diverge policy from Tempo | Reject policy writes and restore policy reads from Tempo as the sole source of truth |

### Zone Execution and Privacy

| ID | Assertion | Tier | Crit | Impact | Recovery |
|---|---|---|---|---|---|
| `TEMPO-ZONE-ADVANCE-TEMPO-FIRST` | When present, `advanceTempo` is the first transaction in a zone block | Tip-critical | 🟡 | User transactions can execute against the wrong Tempo binding or stale config | Reject the block witness and rebuild with system transaction ordering enforced |
| `TEMPO-ZONE-CONTRACT-CREATION-DISABLED` | User `CREATE` and `CREATE2` always revert on zones | Tip-critical | 🟡 | Users can deploy contracts that bypass privacy and system-token assumptions | Reject the transaction and verify EVM config disables creation in execution and simulation paths |
| `TEMPO-ZONE-BALANCE-ALLOWANCE-PRIVACY` | `balanceOf` and `allowance` reveal values only to authorized callers or the sequencer | Tip-critical | 🟡 | Account balances and approvals leak through token precompiles | Patch the token precompile, audit RPC exposure, and notify affected users if values leaked |
| `TEMPO-ZONE-FIXED-TOKEN-GAS` | TIP-20 transfer and approve operations charge fixed gas independent of account storage layout | Audit-only | 🟢 | Gas timing leaks whether addresses have prior token activity | Patch the gas schedule and add differential gas tests for fresh vs existing accounts |
| `TEMPO-ZONE-BLOCK-TIMESTAMP-MONOTONIC` | Zone block timestamps are non-decreasing and block numbers increment by one | Tip-critical | 🟢 | Time-dependent application logic and proof replay assumptions can break | Reject the block witness and restart payload building from the last accepted header |

### Private RPC

| ID | Assertion | Tier | Crit | Impact | Recovery |
|---|---|---|---|---|---|
| `TEMPO-ZONE-RPC-TOKEN-DOMAIN` | Authorization tokens bind to `TempoZoneRPC`, version, zone ID or wildcard zero, chain ID, `issuedAt`, and `expiresAt` | Tip-critical | 🟡 | Tokens can be replayed across zones, chains, or protocol versions | Reject tokens with bad domains and rotate any exposed session keys |
| `TEMPO-ZONE-RPC-TOKEN-LIFETIME` | Tokens expire, are not valid too far in the future, and never exceed the 30-day maximum validity window | Tip-critical | 🟡 | Long-lived or future-dated tokens can preserve unauthorized access | Reject the token, require re-authentication, and audit server clock drift |
| `TEMPO-ZONE-RPC-SENDER-SCOPING` | `eth_sendRawTransaction`, `eth_call`, and `eth_estimateGas` require the authenticated account to match the transaction or call sender | Tip-critical | 🟡 | Users can simulate or submit transactions as other accounts | Reject mismatches and alert on repeated sender-scope failures |
| `TEMPO-ZONE-RPC-RAW-STATE-SEQUENCER-ONLY` | Raw state, full transaction/block, debug/admin/txpool, proof, and pending-transaction methods are unavailable to non-sequencers | Tip-critical | 🟡 | Private transaction, storage, and mempool data leaks | Disable the method, rotate exposed credentials if needed, and audit logs for leaked requests |
| `TEMPO-ZONE-RPC-BLOCK-REDACTION` | Non-sequencer block responses have empty `transactions` and zeroed `logsBloom` | Tip-critical | 🟡 | Users can infer other users' activity from block payloads or bloom probes | Patch response shaping and invalidate cached unredacted responses |
| `TEMPO-ZONE-RPC-LOG-SCOPING` | Log queries and subscriptions only return TIP-20 events where the authenticated account is a relevant party | Tip-critical | 🟡 | Users can observe other users' transfers, approvals, mints, or burns | Tighten filter injection/post-filtering and audit historical logs served by the RPC |
| `TEMPO-ZONE-RPC-TIMING-FLOOR` | Scoped data-fetching methods enforce the minimum response time before returning negative results | Audit-only | 🟢 | Timing differences leak transaction or log existence | Restore the timing floor and run side-channel tests for authorized vs unauthorized queries |
| `TEMPO-ZONE-RPC-KEYCHAIN-REVOCATION` | Keychain-authenticated WebSocket connections terminate within one second of importing a revocation block | Tip-critical | 🟡 | Revoked session keys can keep observing private zone activity | Disconnect affected sockets, refresh keychain state, and alert on stale authenticated connections |
