# Tempo Zones

**Table of Contents**

- [Abstract](#abstract)
- [Specification](#specification)
  - [Terminology](#terminology)
  - [System Overview](#system-overview)
  - [Zone Deployment](#zone-deployment)
    - [Chain ID](#chain-id)
    - [Tempo Contracts](#tempo-contracts)
    - [Zone Predeploys](#zone-predeploys)
    - [Zone Token Model](#zone-token-model)
  - [Sequencer Operations](#sequencer-operations)
    - [Token Management](#token-management)
    - [Gas Rate Configuration](#gas-rate-configuration)
    - [Encryption Key Management](#encryption-key-management)
    - [Sequencer Transfer](#sequencer-transfer)
  - [Deposits](#deposits)
    - [Regular Deposits](#regular-deposits)
    - [Deposit Fees](#deposit-fees)
    - [Deposit Queue](#deposit-queue)
    - [Encrypted Deposits](#encrypted-deposits)
    - [Onchain Decryption Verification](#onchain-decryption-verification)
    - [Deposit Failures and Bounce-Back](#deposit-failures-and-bounce-back)
  - [Withdrawals](#withdrawals)
    - [Withdrawal Request](#withdrawal-request)
    - [Withdrawal Fees](#withdrawal-fees)
    - [Withdrawal Batching](#withdrawal-batching)
    - [Withdrawal Queue](#withdrawal-queue)
    - [Withdrawal Processing](#withdrawal-processing)
    - [Withdrawal Callbacks](#withdrawal-callbacks)
    - [Withdrawal Failures and Bounce-Back](#withdrawal-failures-and-bounce-back)
    - [Authenticated Withdrawals](#authenticated-withdrawals)
    - [Zone-to-Zone Transfers](#zone-to-zone-transfers)
  - [Zone Execution](#zone-execution)
    - [Fee Accounting](#fee-accounting)
    - [Block Structure](#block-structure)
    - [Block Header Format](#block-header-format)
    - [Privacy Modifications](#privacy-modifications)
  - [Tempo State Reads](#tempo-state-reads)
    - [TempoState Predeploy](#tempostate-predeploy)
    - [Header Finalization](#header-finalization)
    - [Storage Reads](#storage-reads)
    - [Staleness and Finality](#staleness-and-finality)
  - [TIP-403 Policies](#tip-403-policies)
    - [Policy Enforcement on Zones](#policy-enforcement-on-zones)
    - [Policy Inheritance](#policy-inheritance)
  - [Private RPC](#private-rpc)
    - [Authorization Tokens](#authorization-tokens)
    - [Signature Types](#signature-types)
    - [Method Access Control](#method-access-control)
    - [Block Responses](#block-responses)
    - [Event Filtering](#event-filtering)
    - [Timing Side Channels](#timing-side-channels)
    - [WebSocket Subscriptions](#websocket-subscriptions)
    - [Zone-Specific Methods](#zone-specific-methods)
    - [Error Codes](#error-codes)
  - [Proving System](#proving-system)
    - [State Transition Function](#state-transition-function)
    - [Witness Structure](#witness-structure)
    - [Input Schematic](#input-schematic)
    - [Detailed Input Definitions](#detailed-input-definitions)
    - [Shared Trie Proof Format](#shared-trie-proof-format)
    - [Batch Output](#batch-output)
    - [Block Execution](#block-execution-stateless-prover-execution-function)
    - [Tempo State Proofs](#tempo-state-proofs)
    - [Deployment Modes](#deployment-modes)
  - [Batch Submission](#batch-submission)
    - [submitBatch](#submitbatch)
    - [Verifier Interface](#verifier-interface)
    - [Anchor Block Validation](#anchor-block-validation)
    - [Proof Requirements](#proof-requirements)
  - [Zone Precompiles](#zone-precompiles)
    - [TIP-20 Token Precompile](#tip-20-token-precompile)
    - [Chaum-Pedersen Verify](#chaum-pedersen-verify)
    - [AES-GCM Decrypt](#aes-gcm-decrypt)
  - [Contracts and Interfaces](#contracts-and-interfaces)
    - [Common Types](#common-types)
    - [IZoneFactory](#izonefactory)
    - [IZonePortal](#izoneportal)
    - [IZoneMessenger](#izonemessenger)
    - [IWithdrawalReceiver](#iwithdrawalreceiver)
    - [ITempoState](#itempostate)
    - [IZoneInbox](#izoneinbox)
    - [IZoneOutbox](#izoneoutbox)
    - [IZoneConfig](#izoneconfig)
    - [TIP-403 Registry](#tip-403-registry)
  - [Network Upgrades and Hard Fork Activation](#network-upgrades-and-hard-fork-activation)

---

# Abstract

A Tempo Zone is a private execution environment anchored to Tempo. Inside a zone, balances, transfers, and transaction history are invisible to block explorers, indexers, and other users. Each zone is operated by a dedicated sequencer that is the sole block producer, settling back to Tempo through a proof-agnostic verification system.

Funds enter a zone through deposits on Tempo, where they are locked in the portal. The zone mints equivalent tokens, and users transact privately with balances and transaction history hidden behind authenticated RPC access and execution-level controls. When users withdraw, tokens are burned on the zone and released from the portal on Tempo. Proofs guarantee that the sequencer executed every transaction correctly and cannot forge state transitions. Withdrawals support optional callbacks, making them composable with Tempo contracts and enabling zone-to-zone transfers.

This document specifies the zone protocol: deployment, sequencer operations, deposits, execution, the private RPC interface, the proving system, batch submission, withdrawals, precompiles, contract interfaces, and the network upgrade process.

# Specification

## Terminology

| Term | Definition |
|------|------------|
| Tempo | The base chain that zones settle to. |
| Zone | A private execution environment anchored to Tempo. |
| Portal | The contract on Tempo that locks deposited tokens and finalizes withdrawals for a zone. |
| Batch | A sequencer-produced commitment covering one or more zone blocks, submitted to Tempo with a proof. |
| Enabled token | A TIP-20 token that the sequencer has activated for deposits and withdrawals on a zone. Enablement is permanent. |
| TIP-20 | Tempo's fungible token standard. |
| TIP-403 | Tempo's compliance registry. Issuers attach transfer policies (whitelists, blacklists) to TIP-20 tokens. |
| Predeploy | A system contract deployed at a fixed address on the zone at genesis. |

<br>

## System Overview

Each zone is operated by a **sequencer** that collects transactions, produces blocks, generates proofs, and submits batches to Tempo. A single registered address controls sequencer operations for each zone. **Users** deposit TIP-20 tokens from Tempo into the zone, transact privately, and withdraw back to Tempo.

On the Tempo side, an onchain **verifier** contract validates that each batch was executed correctly. The verifier is abstracted behind a minimal interface (`IVerifier`) and is proof-agnostic. Any proving backend (ZK, TEE, or otherwise) can implement the interface. The portal does not care how the proof was produced.

On Tempo, each zone has a **portal** that locks deposited tokens. When a user deposits, the portal locks their tokens and appends the deposit to a queue. The sequencer observes the deposit, advances the zone's view of Tempo, and mints equivalent tokens on the zone.

Users transact on the zone privately. Balances, transfers, and transaction history are only visible to the account holder and the sequencer. The zone does not post transaction data, and data availability is entrusted to the sequencer. The sequencer has full visibility into zone activity. Privacy protects against public observers on Tempo, not against the sequencer.

Zones rely on the following trust assumptions: the verifier must be sound for state transition integrity, the sequencer is trusted for liveness and data availability, and there is no forced inclusion or permissionless exit mechanism.

When a user wants to exit, they request a withdrawal on the zone. Their tokens are burned on the zone side, and the withdrawal is added to a pending list. At the end of a batch, the sequencer finalizes all pending withdrawals into a hash chain and generates a proof covering the full batch of zone blocks. The sequencer submits this batch and proof to the portal on Tempo, which verifies the proof and queues the withdrawals. The sequencer then processes each withdrawal, releasing tokens from the portal to the recipient.

```mermaid
sequenceDiagram
    participant U as User
    participant T as Tempo
    participant Z as Zone

    Note over T: Deposit
    U->>T: ZonePortal.deposit()
    T->>T: lock tokens, append to deposit queue

    Note over Z: Process deposit
    Z-->>T: observe DepositMade
    Z->>Z: ZoneInbox.advanceTempo()
    Z->>Z: mint tokens to recipient

    U->>Z: transact privately

    Note over Z: Withdrawal
    U->>Z: ZoneOutbox.requestWithdrawal()
    Z->>Z: burn tokens, finalize batch

    Note over T: Settlement
    Z->>T: ZonePortal.submitBatch()
    T->>T: verify proof, queue withdrawals

    Note over T: Withdraw
    Z->>T: ZonePortal.processWithdrawal()
    T->>U: release tokens
```

<br>

## Zone Deployment

A zone is created via `ZoneFactory.createZone(...)` on Tempo with the following parameters:

| Parameter | Description |
|-----------|-------------|
| `initialToken` | The first TIP-20 token to enable. The sequencer can enable additional tokens later. |
| `sequencer` | The address that will operate the zone. |
| `verifier` | The `IVerifier` contract used to validate batch proofs. |
| `zoneParams` | Genesis configuration: genesis block hash, genesis Tempo block hash, and genesis Tempo block number. |

The factory assigns a unique `zoneId`, deploys a [`ZonePortal`](#izoneportal) and a [`ZoneMessenger`](#izonemessenger), and enables the initial token. The [`ZoneCreated`](#izonefactory) event emits all deployment parameters.

### Chain ID

Each zone has a unique chain ID derived from its zone ID:

```
chain_id = 421700000 + zone_id
```

The prefix `4217` is derived from the Tempo chain ID. This ensures replay protection between zones. A transaction signed for one zone cannot be replayed on another. The chain ID is set in the zone's genesis configuration and validated by the zone node at startup.

### Tempo Contracts

A single [`ZoneFactory`](#izonefactory) on Tempo creates zones and maintains the registry of all deployed zones. When a zone is created, the factory deploys two contracts for it:

| Contract | Purpose |
|----------|---------|
| [`ZonePortal`](#izoneportal) | Locks deposited tokens, accepts batch submissions, verifies proofs, and processes withdrawals. Manages the token registry and deposit/withdrawal queues. |
| [`ZoneMessenger`](#izonemessenger) | Relays withdrawal callbacks. When a withdrawal includes calldata, the messenger transfers tokens from the portal to the recipient and executes the callback atomically. Deployed separately from the portal to isolate callback execution. |

The portal gives the messenger max approval for each enabled token so that withdrawal callbacks can transfer tokens from the portal to the recipient in a single call.

### Zone Predeploys

Each zone has six system contracts deployed at genesis at fixed addresses:

| Predeploy | Address | Purpose |
|-----------|---------|---------|
| [`TempoState`](#itempostate) | `0x1c00...0000` | Stores finalized Tempo block headers and provides storage read access to Tempo contracts. |
| [`ZoneInbox`](#izoneinbox) | `0x1c00...0001` | Advances the zone's view of Tempo and processes incoming deposits. Sole mint authority. |
| [`ZoneOutbox`](#izoneoutbox) | `0x1c00...0002` | Handles withdrawal requests and batch finalization. Sole burn authority. |
| [`ZoneConfig`](#izoneconfig) | `0x1c00...0003` | Central configuration. Reads the sequencer address and token registry from Tempo via `TempoState`. |
| `TempoStateReader` | `0x1c00...0004` | Precompile stub for reading Tempo L1 storage. Actual reads are performed by the zone node and validated against the `tempoStateRoot`. |
| `ZoneTxContext` | `0x1c00...0005` | Provides the current transaction hash to system contracts (used by `ZoneOutbox` for `senderTag` computation). |

`ZoneConfig` reads the sequencer address and token registry from the portal on Tempo via `TempoState` storage reads, making Tempo the single source of truth for zone configuration. See [Tempo State Reads](#tempo-state-reads) for details.

### Zone Token Model

Contract creation is disabled on zones (`CREATE` and `CREATE2` revert). All TIP-20 tokens on a zone are representations of Tempo tokens, deployed at the same address as on Tempo. When the sequencer enables a token on the portal, the zone's TIP-20 factory precompile (at `0x20Fc000000000000000000000000000000000000`) provisions a TIP-20 token precompile at that address. The factory is called by `ZoneInbox` during `advanceTempo` and is not user-accessible.

Token supply on the zone is controlled exclusively by the system contracts:

- `ZoneInbox` mints tokens when processing deposits from Tempo.
- `ZoneOutbox` burns tokens when users request withdrawals.

The zone-side supply of each token always equals net deposits minus net withdrawals. The corresponding tokens on Tempo are locked in the portal. No other actor can mint or burn zone tokens.

<br>

## Sequencer Operations

### Token Management

The sequencer manages which TIP-20 tokens are available on the zone:

- `enableToken(token)`: Enable a new TIP-20 for deposits and withdrawals. This is **irreversible**. Once enabled, a token can never be disabled.
- `pauseDeposits(token)`: Pause new deposits for a token. Does not affect withdrawals.
- `resumeDeposits(token)`: Resume deposits for a previously paused token.

The portal maintains a `TokenConfig` per token with an `enabled` flag and a configurable `depositsActive` flag, along with an append-only `enabledTokens` list. The sequencer can halt deposits but cannot disable withdrawals for an enabled token. Note that token issuers can independently restrict transfers via TIP-403 policies, which may cause withdrawals to fail and bounce back (see [Withdrawal Failures and Bounce-Back](#withdrawal-failures-and-bounce-back)).

### Gas Rate Configuration

The sequencer configures gas rates that determine fees for deposits, withdrawals, and bounce-backs. Each rate is the price (in token units) of one gas unit on the chain where the work runs:

| Rate | Set via | Used for |
|------|---------|----------|
| `zoneGasRate` | `ZonePortal.setZoneGasRate()` | Deposit fees: `FIXED_DEPOSIT_GAS (100,000) * zoneGasRate` |
| `tempoGasRate` (portal) | `ZonePortal.setTempoGasRate()` | Deposit bounce-back fees: `FIXED_BOUNCEBACK_GAS (300,000) * tempoGasRate` |
| `tempoGasRate` (outbox) | `ZoneOutbox.setTempoGasRate()` | Withdrawal fees: `(WITHDRAWAL_BASE_GAS (50,000) + gasLimit) * tempoGasRate` |

`zoneGasRate` is read on Tempo (the portal is the source of truth for zone-side gas pricing seen by users when depositing). The portal's `tempoGasRate` is also read on Tempo at deposit time, where it prices the Tempo-side bounce-back transfer that may eventually run there. The outbox's `tempoGasRate` is read on the zone at withdrawal-request time, where it prices the Tempo-side withdrawal that the sequencer will eventually process. The two `tempoGasRate` settings are stored independently on different chains and are not automatically synchronized; the sequencer is responsible for keeping them aligned with their respective Tempo gas-cost models. The sequencer does not have to set the same value for both of them.

All rates are denominated in token units per gas unit. A single uniform set of rates applies to all tokens. Fees are paid in the same token being deposited or withdrawn.

The sequencer takes the risk on gas-price fluctuations for both `tempoGasRate` values. If actual gas costs on Tempo exceed the fee collected, the sequencer covers the difference; if costs are lower, the sequencer keeps the surplus.

### Encryption Key Management

The sequencer publishes a secp256k1 encryption public key used for [encrypted deposits](#encrypted-deposits). The key is set via `setSequencerEncryptionKey(x, yParity, popV, popR, popS)` on the portal, which requires a proof of possession (an ECDSA signature proving control of the corresponding private key).

The portal stores all historical encryption keys in an append-only list. Users specify a `keyIndex` when making encrypted deposits, referencing which key they encrypted to. This avoids a race condition where a key rotates between transaction signing and block inclusion.

When a new key is set, the previous key remains valid for `ENCRYPTION_KEY_GRACE_PERIOD` (86,400 blocks). After that, deposits using the old key are rejected. The current key never expires. Users can call `isEncryptionKeyValid(keyIndex)` before signing to check validity.

### Sequencer Transfer

The sequencer can transfer control to a new address via a two-step process on Tempo:

1. Current sequencer calls `ZonePortal.transferSequencer(newSequencer)` to nominate a new sequencer.
2. New sequencer calls `ZonePortal.acceptSequencer()` to accept the transfer.

Sequencer management happens exclusively on Tempo. Zone-side contracts read the sequencer address from the portal via `ZoneConfig`, so the transfer takes effect on the zone once `advanceTempo` imports the Tempo block containing the accepted transfer. The two-step pattern prevents accidental transfers to incorrect addresses.

<br>

## Deposits

Deposits move TIP-20 tokens from Tempo into a zone. The user deposits on Tempo, the portal locks the tokens and appends the deposit to a hash chain, and the sequencer mints equivalent tokens on the zone.

### Regular Deposits

A user deposits by calling `deposit(token, to, amount, memo, bouncebackRecipient)` on the portal. The portal:

1. Validates the token is enabled and deposits are active.
2. Requires `bouncebackRecipient != address(0)` (reverts otherwise) and validates `bouncebackRecipient` against the token's TIP-403 policy.
3. Snapshots the deposit fee at the current `zoneGasRate` and the bounce-back fee at the current portal-side `tempoGasRate` (see [Deposit Fees](#deposit-fees)), and requires `amount >= depositFee + bouncebackFee` (reverts `DepositTooSmall` otherwise). The bounce-back fee covers the worst-case Tempo gas of paying out a refund (including new-account creation for `bouncebackRecipient`), so it is priced in Tempo gas, not zone gas.
4. Transfers `amount` from the user into the portal.
5. Pays the `depositFee` to the sequencer immediately. The `bouncebackFee` is reserved on the queued entry; it is only consumed if the deposit later bounces back.
6. Appends the deposit to the deposit queue hash chain with the net amount (`amount - depositFee`), `bouncebackRecipient`, and the snapshotted `bouncebackFee`.
7. Emits `DepositMade`.

The sequencer observes `DepositMade` events and relays deposits to the zone via `ZoneInbox.advanceTempo()`. This function processes deposits in order, minting the zone-side TIP-20 token to each recipient: `mint(deposit.to, deposit.amount)`.

If the zone-side mint reverts (for example, because the recipient is blocked by a TIP-403 policy active on the zone at processing time), the deposit bounces back to `bouncebackRecipient` on Tempo. See [Deposit Failures and Bounce-Back](#deposit-failures-and-bounce-back) for the full mechanism. If the sequencer withholds deposits, funds remain locked in the portal with no forced inclusion mechanism.

```mermaid
sequenceDiagram
    participant U as User
    participant T as Tempo
    participant Z as Zone

    U->>T: ZonePortal.deposit()
    T->>T: append to depositQueue
    Note over T: emit DepositMade
    Z-->>T: observe DepositMade
    Z->>Z: ZoneInbox.advanceTempo()
    Z->>Z: process deposit
    Z->>Z: TIP20.mint(to, amount)
```

### Deposit Fees

Every deposit is associated with two separate fees, both paid in the same token being deposited but priced at different gas rates because the work they cover runs on different chains:

```
depositFee    = FIXED_DEPOSIT_GAS    * zoneGasRate    (= 100,000 * zoneGasRate)
bouncebackFee = FIXED_BOUNCEBACK_GAS * tempoGasRate   (= 300,000 * tempoGasRate)
```

`zoneGasRate` and `tempoGasRate` are both portal-local sequencer-managed rates set via `ZonePortal.setZoneGasRate()` and `ZonePortal.setTempoGasRate()` respectively (see [Gas Rate Configuration](#gas-rate-configuration)). The portal's `tempoGasRate` is independent of the `tempoGasRate` stored on `ZoneOutbox` for withdrawal pricing.

- The **deposit fee** covers the sequencer's cost of processing the deposit on the zone (calling `advanceTempo`, performing the mint, advancing the queue) and is therefore priced at the zone's gas rate. It is charged on every deposit, success or failure, and paid to the sequencer immediately on Tempo.
- The **bounce-back fee** covers the sequencer's worst-case Tempo-side cost of paying out a refund — primarily new-account creation for `bouncebackRecipient`, which can dominate the gas of `processWithdrawal` and is much larger than the steady-state per-deposit gas — and is priced at Tempo's gas rate. It is charged only when a deposit actually bounces back, and is paid to the sequencer at that point.

### Deposit Queue

Deposits flow from Tempo to the zone through a hash chain. The portal tracks a single `currentDepositQueueHash` representing the head of the chain. Each new deposit wraps the existing hash:

```
currentDepositQueueHash = keccak256(abi.encode(DepositType.Regular, deposit, currentDepositQueueHash))
```

The newest deposit is always outermost, making onchain addition O(1). The zone tracks its own `processedDepositQueueHash` in state. During `advanceTempo()`, the zone processes deposits oldest-first, rebuilding the hash chain and validating that the result matches `currentDepositQueueHash` read from Tempo state via `TempoState.readTempoStorageSlot()`.

After a batch is accepted, the portal updates `lastSyncedTempoBlockNumber` to record how far Tempo state was synced. Users can check whether their deposit has been processed by comparing their deposit's Tempo block number against this value.

### Encrypted Deposits

Users can encrypt the recipient and memo of a deposit so that only the sequencer can see who received the funds. The token, sender, and amount remain public (required for onchain accounting), but the `to` address and `memo` are encrypted.

The encryption scheme is ECIES with secp256k1:

1. The user generates an ephemeral keypair and derives a shared secret via ECDH with the sequencer's published encryption key.
2. The user derives an AES-256 key from the shared secret using HKDF-SHA256.
3. The user encrypts `(to || memo || padding)` with AES-256-GCM, producing ciphertext, a nonce, and an authentication tag.
4. The user calls `depositEncrypted(token, amount, keyIndex, encryptedPayload, bouncebackRecipient)` on the portal, where `keyIndex` references which encryption key they encrypted to (see [Encryption Key Management](#encryption-key-management)), and `bouncebackRecipient` is the Tempo address that receives a refund if zone-side processing fails (see [Deposit Failures and Bounce-Back](#deposit-failures-and-bounce-back)). Like `deposit()`, `depositEncrypted` requires `bouncebackRecipient != address(0)` (reverts otherwise) and `amount >= depositFee + bouncebackFee` (reverts `DepositTooSmall` otherwise), and snapshots the bounce-back fee on the queued deposit. The encrypted-deposit case makes the `bouncebackRecipient` requirement particularly important because a ciphertext that fails onchain decryption verification has no well-defined recipient on the zone, so a zero refund target would permanently stall the deposit queue.

The portal locks the tokens, appends the encrypted deposit to the deposit queue, and emits `EncryptedDepositMade`. The sequencer decrypts the payload off-chain and provides the decrypted `(to, memo)` when processing the deposit on the zone via `advanceTempo()`.

Regular and encrypted deposits share a single ordered queue with a type discriminator in the hash:

```
keccak256(abi.encode(DepositType.Regular, deposit, prevHash))
keccak256(abi.encode(DepositType.Encrypted, encryptedDeposit, prevHash))
```

Deposits are processed in the exact order they were made, regardless of type.

| Field | Visibility | Reason |
|-------|------------|--------|
| `token` | Public | Required for onchain accounting and zone-side minting |
| `sender` | Public | Required for onchain accounting and as the origin of the deposit event |
| `amount` | Public | Required for onchain accounting |
| `bouncebackRecipient` | Public | Required; receives the Tempo-side refund if decryption or the final mint fails |
| `to` | Encrypted | Only the sequencer learns the recipient |
| `memo` | Encrypted | Only the sequencer learns the payment context |

### Onchain Decryption Verification

When the sequencer processes an encrypted deposit on the zone, it claims the ciphertext decrypts to a specific `(to, memo)`. The zone verifies this onchain without the sequencer revealing their private key.

The sequencer provides the ECDH shared secret alongside the decrypted data. Verification proceeds in three steps:

1. **Chaum-Pedersen proof.** The sequencer provides a zero-knowledge proof that the shared secret was correctly derived: "I know `privSeq` such that `pubSeq = privSeq * G` AND `sharedSecretPoint = privSeq * ephemeralPub`." The [Chaum-Pedersen Verify](#chaum-pedersen-verify) precompile checks this proof. The sequencer's public key is looked up from the onchain key history, not supplied by the sequencer, preventing key substitution.

2. **AES-GCM decryption.** The zone derives an AES-256 key from the shared secret using HKDF-SHA256 (implemented in Solidity using the SHA256 precompile at `0x02`). The HKDF info string includes `tempoPortal`, `keyIndex`, and `ephemeralPubkeyX` for domain separation. The [AES-GCM Decrypt](#aes-gcm-decrypt) precompile decrypts the ciphertext and validates the GCM authentication tag.

3. **Plaintext validation.** The zone confirms the decrypted plaintext matches the `(to, memo)` the sequencer claimed. The plaintext is packed as `[address (20 bytes)][memo (32 bytes)][padding (12 bytes)]` totaling 64 bytes.

If any step fails (invalid proof, GCM tag mismatch, plaintext mismatch), the zone does **not** attempt any zone-side mint. Instead, the deposit bounces back immediately to `bouncebackRecipient` on Tempo via the outbox (see [Deposit Failures and Bounce-Back](#deposit-failures-and-bounce-back)). Because `depositEncrypted` requires a non-zero `bouncebackRecipient` at deposit time, this path always has a well-defined target and never stalls the deposit queue.

The verification above is only performed when the sequencer accepts an encrypted deposit. If the sequencer marks the deposit as rejected via `QueuedDeposit.rejected = true` (see [Sequencer rejection](#sequencer-rejection)), all three steps are skipped and the inbox enqueues a bounce-back without invoking the cryptographic precompiles or consuming a `DecryptionData` entry.

The Chaum-Pedersen proof also prevents griefing. Without it, a user could submit garbage ciphertext that the sequencer cannot decrypt and cannot prove invalid, blocking the chain. The proof lets the sequencer demonstrate correct shared secret derivation, and the GCM tag failure then proves the ciphertext itself was invalid.

```mermaid
sequenceDiagram
    participant U as User
    participant T as Tempo
    participant Z as Zone

    U->>T: ZonePortal.depositEncrypted(..., bouncebackRecipient)
    Note over T: require bouncebackRecipient != address(0)
    T->>T: check TIP-403 for bouncebackRecipient
    T->>T: append to depositQueue
    Note over T: emit EncryptedDepositMade
    Z-->>T: observe EncryptedDepositMade
    Z->>Z: ZoneInbox.advanceTempo(..., QueuedDeposit{rejected})
    alt sequencer rejects
        Note over Z: skip onchain decryption verification
        Z->>T: bounce back to bouncebackRecipient via withdrawal queue
    else sequencer accepts
        Z->>Z: onchain decryption (Chaum-Pedersen + AES-GCM)
        alt verification succeeds
            Z->>Z: TIP20.mint(decryptedTo, amount)
            Note over Z: if mint reverts
            Z->>T: bounce back to bouncebackRecipient via withdrawal queue
        else verification fails
            Note over Z: no zone-side mint attempted
            Z->>T: bounce back to bouncebackRecipient via withdrawal queue
        end
    end
```

### Deposit Failures and Bounce-Back

> Related TIP: the guaranteed-liveness property of the Tempo-side refund transfer described below relies on [TIP-1049: System-Contract Transfer Policy Exemption](https://github.com/tempoxyz/tempo/blob/main/tips/tip-1049.md), which introduces `ITIP20.systemForceTransfer` and a `ZoneFactory`-gated authority predicate that covers every per-zone `ZonePortal`. Before TIP-1049 activates, the mechanism described here is still correct with respect to safety, but a refund transfer can still revert if the token's TIP-403 policy is edited to forbid `bouncebackRecipient` between deposit time and refund time.

Regular deposits always succeed on the original design: the zone simply mints to `deposit.to`. In practice the mint can revert. The most important case is TIP-403 policies: the policy for a token can change between the time the depositor sent funds on Tempo and the time the zone processes the deposit, and the new policy may forbid minting to `deposit.to`. Without a recovery path, those funds would be stranded — locked in the portal on Tempo, un-minted on the zone.

To address this symmetrically to [Withdrawal Failures and Bounce-Back](#withdrawal-failures-and-bounce-back), every deposit carries a `bouncebackRecipient`: a Tempo address that receives a refund if zone-side processing fails.

**Validation at deposit time.** Both `deposit(...)` and `depositEncrypted(...)` require `bouncebackRecipient != address(0)` and revert otherwise (`MissingBouncebackRecipient`). The address must also be authorized by the token's current TIP-403 policy as a recipient.

Checking the TIP-403 policy at deposit time guarantees that a later bounce-back transfer on Tempo will not itself revert on policy grounds. The check uses the portal's view of the policy at the time of deposit; later policy changes do not invalidate already-queued deposits.

The portal-internal withdrawal-bounce-back path (`_enqueueWithdrawalBounceBack`) constructs a `Deposit` directly and bypasses the user-facing entry points, so it can — and does — set `bouncebackRecipient = address(0)` as a sentinel that marks an internal one-shot deposit; see **No recursive bounces** below.

**Triggering conditions.** There are three triggering sites:

- **Regular deposit, mint reverts.** `ZoneInbox.advanceTempo` calls `TIP20.mint(deposit.to, deposit.amount)`. If that mint reverts the deposit bounces back to the user-supplied `bouncebackRecipient`. The user-facing `deposit(...)` entry point ensures `bouncebackRecipient != address(0)`, so the queue can always advance past a failed mint.
- **Encrypted deposit.** Two failure modes, both of which unconditionally bounce back (no zone-side mint is attempted as a fallback):
  - **Invalid encryption.** The Chaum-Pedersen proof, AES-GCM tag, or plaintext comparison fails during [Onchain Decryption Verification](#onchain-decryption-verification). There is no well-defined recipient on the zone in this case, so the zone does not try to mint to the depositor; it bounces back immediately.
  - **Valid decryption, mint reverts.** `TIP20.mint(decryptedTo, amount)` reverts (for example, because a TIP-403 policy active on the zone forbids minting to the decrypted recipient, or a custom TIP-20 `mint` reverts for some token-specific reason). The deposit bounces back.
- **Sequencer rejection.** The sequencer can mark any user-initiated deposit (regular or encrypted) as rejected when calling `advanceTempo` (see [Sequencer rejection](#sequencer-rejection) below). The zone treats the deposit as if it had failed: it skips the zone-side mint entirely (and, for encrypted deposits, the onchain decryption verification) and enqueues a bounce-back to `bouncebackRecipient`.

Because both deposit entry points require a non-zero `bouncebackRecipient`, every user-initiated deposit has a refund target and the deposit queue never stalls on a failed mint, invalid encryption, or sequencer rejection.

The portal's internal withdrawal-bounce-back deposits are the only entries with `bouncebackRecipient == address(0)`. They are introduced by `_enqueueWithdrawalBounceBack` after a withdrawal callback fails, and their zone-side mint is treated as infallible (see **No recursive bounces** below). The sequencer cannot reject these entries: a `rejected` flag on an internal withdrawal-bounce-back deposit is silently ignored and the deposit is processed as if not rejected, preserving the terminal-bounce invariant.

**Zone-side handling.** When an encrypted deposit fails (either branch), when a regular deposit's mint reverts, or when the sequencer rejects a deposit, the `ZoneInbox` calls `ZoneOutbox.enqueueDepositBounceBack(token, amount, bouncebackRecipient)`. For a regular deposit whose mint reverted the revert is caught in a `try/catch`; for an encrypted deposit with invalid encryption no mint is attempted and the inbox enqueues the bounce-back directly; for a sequencer-rejected deposit the inbox skips processing entirely and enqueues the bounce-back without ever calling the token contract (and, for encrypted deposits, without performing onchain decryption verification). `enqueueDepositBounceBack` records a zero-fee, zero-callback, zero-`fallbackRecipient` withdrawal in the outbox's pending list, with `sender = address(0)` and `txHash = bytes32(0)`. The inbox emits one of three events so off-chain observers can distinguish the failure mode: `DepositFailed` (regular deposit whose mint reverted), `EncryptedDepositFailed` (encrypted deposit, either invalid encryption or mint revert), or `DepositRejected` (sequencer-initiated rejection, regardless of deposit type). The deposit queue hash chain advances normally; no retries are performed on the zone.

**Tempo-side refund.** The bounce-back withdrawal is submitted in the next batch alongside any user-initiated withdrawals. When `ZonePortal.processWithdrawal` runs on the deposit-bounce-back entry (`gasLimit == 0`, `fee == 0`, `fallbackRecipient == address(0)`), it deducts the snapshotted `bouncebackFee` from the queued amount, transfers `amount - bouncebackFee` of `token` from the portal's escrow to `bouncebackRecipient` on Tempo, and pays `bouncebackFee` to the sequencer to compensate for the gas cost of the bounce-back transfer (which can include new-account creation for `bouncebackRecipient`). `bouncebackRecipient` was validated against the token's TIP-403 policy at deposit time, so that specific policy cannot cause the refund to fail. However, TIP-403 policies are mutable between deposit time and refund time, and the standard `ITIP20.transfer` does not exempt system contracts from the current policy. The guaranteed-liveness property of the refund therefore depends on [TIP-1049 (System-Contract Transfer Policy Exemption)](https://github.com/tempoxyz/tempo/blob/main/tips/tip-1049.md) activating: once TIP-1049 is live, `ZonePortal.processWithdrawal` uses `ITIP20.systemForceTransfer` on this path, which skips the TIP-403 check while preserving pause, zero-recipient, balance, and spending-limit enforcement. Until TIP-1049 activates, the refund transfer can still revert if the policy is edited to forbid `bouncebackRecipient` after the deposit; in that case the bounce-back re-enters the pending list and is retried on subsequent batches (the `bouncebackFee` deduction is idempotent — it is computed from the snapshot stored on the queued deposit). The sequencer keeps the deposit fee that was already paid on Tempo regardless of outcome, and additionally collects the bounce-back fee on this path.

**Sequencer rejection.** When calling `advanceTempo`, the sequencer can mark any individual deposit as rejected by setting `QueuedDeposit.rejected = true` for that entry. A rejected deposit is processed exactly like a deposit-time failure: the zone skips the zone-side mint and enqueues a bounce-back to `bouncebackRecipient`. For encrypted deposits, rejection short-circuits the cryptographic verification — the sequencer is not required to provide a `DecryptionData` entry for a rejected encrypted deposit and the AES-GCM / Chaum-Pedersen precompiles are not invoked. The 1:1 correspondence between non-rejected encrypted deposits and `DecryptionData` entries still holds.

- A deposit created by the portal as a bounce-back from a failed _withdrawal_ (`_enqueueWithdrawalBounceBack`) always sets `bouncebackRecipient = address(0)`. This is an internal sentinel — the user-facing `deposit()` entry point rejects zero — that tells the zone to mint unconditionally to `fallbackRecipient` and never bounce again, even if the mint reverts. This matches the pre-existing invariant that bounce-back withdrawals are a terminal state.
- A withdrawal created by the zone as a bounce-back from a failed _deposit_ (`enqueueDepositBounceBack`) always sets `fee = 0`, `gasLimit = 0`, `callbackData = ""`, and `fallbackRecipient = address(0)`. The Tempo-side refund transfer is guaranteed-live against TIP-403 policy drift once [TIP-1049](https://github.com/tempoxyz/tempo/blob/main/tips/tip-1049.md) activates. For any non-policy revert (e.g. a paused token), the entry remains in the pending queue and is retried on subsequent batches; the portal never enqueues a second bounce-back for a deposit-bounce-back entry, so the depth of the chain is at most one.

**No user-facing opt-out.** Both `deposit()` and `depositEncrypted()` reject `bouncebackRecipient == address(0)` at deposit time (`MissingBouncebackRecipient`); there is no way for a user-initiated deposit to opt out of the bounce-back path. This preserves liveness of the deposit queue: a failing mint or an invalid encryption can always be recovered by enqueuing a bounce-back, and the queue advances regardless. The `address(0)` value remains reserved as an internal sentinel for the portal-generated withdrawal-bounce-back path described above under **No recursive bounces**.

**Events summary.**

| Event | Emitted by | When |
|-------|------------|------|
| `DepositFailed` | `ZoneInbox` | Mint for a regular deposit reverted, funds queued for bounce-back |
| `EncryptedDepositFailed` | `ZoneInbox` | Encrypted deposit failed — either invalid encryption, or valid decryption with a mint that reverted; funds queued for bounce-back |
| `DepositRejected` | `ZoneInbox` | Sequencer marked the deposit (regular or encrypted) as rejected; funds queued for bounce-back without invoking the token or decryption precompiles |
| `DepositBounceBack` | `ZonePortal` | Bounce-back withdrawal processed on Tempo, funds credited to `bouncebackRecipient` |
| `WithdrawalBounceBack` | `ZonePortal` | Withdrawal-side bounce-back (renamed from `BounceBack` for symmetry with `DepositBounceBack`) |

```mermaid
sequenceDiagram
    participant U as User
    participant T as Tempo
    participant Z as Zone

    U->>T: ZonePortal.deposit(..., bouncebackRecipient)
    Note over T: require bouncebackRecipient != address(0)
    T->>T: check TIP-403 for bouncebackRecipient
    T->>T: append to depositQueue
    Note over T: emit DepositMade
    Z-->>T: observe DepositMade
    Z->>Z: ZoneInbox.advanceTempo(..., QueuedDeposit{rejected})
    alt sequencer rejects
        Z->>Z: ZoneOutbox.enqueueDepositBounceBack()
        Note over Z: emit DepositRejected
    else sequencer accepts
        Z->>Z: try TIP20.mint(deposit.to, amount)
        Note over Z: if mint reverts
        Z->>Z: ZoneOutbox.enqueueDepositBounceBack()
        Note over Z: emit DepositFailed
    end
    Z->>T: ZoneOutbox.finalizeWithdrawalBatch + submitBatch
    T->>T: ZonePortal.processWithdrawal (zero-fee, zero-callback)
    T->>U: TIP20.transfer(bouncebackRecipient, amount)
    Note over T: emit DepositBounceBack
```

<br>

## Withdrawals

Withdrawals move tokens from a zone back to Tempo. The user requests a withdrawal on the zone, tokens are burned, and the sequencer eventually processes the withdrawal on Tempo, releasing tokens from the portal.

### Withdrawal Request

A user withdraws by calling `requestWithdrawal(token, to, amount, memo, gasLimit, fallbackRecipient, data, revealTo)` on the `ZoneOutbox`. The user must first approve the outbox to spend `amount + fee` of the token. The outbox requires `fallbackRecipient != address(0)` (reverts with `InvalidFallbackRecipient`) and validates `fallbackRecipient` against the token's TIP-403 policy on the zone (reverts with `WithdrawalPolicyForbids` if `fallbackRecipient` is not an authorized mint recipient). This is symmetric to the deposit-side validation of `bouncebackRecipient` and ensures that, if the withdrawal later fails on Tempo and bounces back, the zone-side credit to `fallbackRecipient` is guaranteed against the policy seen at request time (see [Withdrawal Failures and Bounce-Back](#withdrawal-failures-and-bounce-back)).

The outbox transfers `amount + fee` from the user via `transferFrom`, burns the tokens, and stores the withdrawal in a pending array. The `WithdrawalRequested` event is emitted with the plaintext sender (zone events are private).

```mermaid
sequenceDiagram
    participant U as User
    participant Z as Zone
    participant T as Tempo

    U->>Z: ZoneOutbox.requestWithdrawal()
    Z->>Z: burn tokens, store pending withdrawal

    Z->>Z: ZoneOutbox.finalizeWithdrawalBatch()
    Z->>T: ZonePortal.submitBatch()
    T->>T: IVerifier.verify()
    T->>T: enqueue withdrawalQueueHash

    Z->>T: ZonePortal.processWithdrawal()
    T->>U: TIP20.transfer(to, amount)

    Note over T: if withdrawal callback
    T->>T: ZoneMessenger.relayMessage()

    Note over T: if failure
    T->>T: bounceBack to fallbackRecipient via deposit queue
```

### Withdrawal Fees

The withdrawal fee compensates the sequencer for Tempo-side gas costs:

```
fee = (WITHDRAWAL_BASE_GAS + gasLimit) * tempoGasRate
    = (50,000 + gasLimit) * tempoGasRate
```

`WITHDRAWAL_BASE_GAS` (50,000) covers the fixed overhead of processing a withdrawal on Tempo (queue dequeue, transfer, event emission). The user specifies `gasLimit` covering any additional execution costs (e.g., callback gas). For simple withdrawals with no callback, use `gasLimit = 0`. The fee is paid in the same token being withdrawn. On success, `amount` goes to the recipient and `fee` goes to the sequencer. On failure (bounce-back), only `amount` is re-deposited to `fallbackRecipient`. The sequencer keeps the fee regardless of outcome.

### Withdrawal Batching

At the end of the final block in a batch, the sequencer calls `finalizeWithdrawalBatch(count, blockNumber, encryptedSenders)` on the `ZoneOutbox`. The `blockNumber` must match the current zone block number. The `encryptedSenders` array carries one ciphertext per finalized withdrawal for [authenticated withdrawals](#authenticated-withdrawals) (empty bytes for withdrawals without `revealTo`). This constructs a hash chain from pending withdrawals in LIFO order (newest to oldest), so the oldest withdrawal ends up outermost, enabling FIFO processing on Tempo:

```
withdrawalQueueHash = EMPTY_SENTINEL
for i from (count - 1) down to 0:
    withdrawalQueueHash = keccak256(abi.encode(withdrawals[i], withdrawalQueueHash))
```

The function writes `withdrawalQueueHash` and `withdrawalBatchIndex` to `lastBatch` storage, where the proof reads them. The call is required even if there are zero withdrawals (use `count = 0`) so the batch index advances. The `withdrawalBatchIndex` ensures batches are submitted in order, preventing the sequencer from omitting batches that contain withdrawals.

### Withdrawal Queue

The portal stores withdrawals in a fixed-size ring buffer with `WITHDRAWAL_QUEUE_CAPACITY = 100`. Each batch gets its own slot.

The portal tracks `head` (oldest unprocessed batch) and `tail` (where the next batch writes). Both are raw counters that never wrap. Modular arithmetic (`index % 100`) is used for slot indexing. Empty slots contain `EMPTY_SENTINEL` (`0xff...ff`) instead of `0x00` to avoid storage clearing and gas refund incentive issues.

When `submitBatch` includes a non-zero `withdrawalQueueHash`, it is written to `slots[tail % 100]` and `tail` advances. The queue reverts with `WithdrawalQueueFull` if `tail - head >= 100`.

### Withdrawal Processing

The sequencer processes withdrawals on Tempo by calling `processWithdrawal(withdrawal, remainingQueue)` on the portal. The portal verifies `keccak256(abi.encode(withdrawal, remainingQueue)) == slots[head % 100]`, then executes the withdrawal.

The withdrawal is popped unconditionally, regardless of success or failure. If `remainingQueue` is zero (last item in the slot), the slot is set to `EMPTY_SENTINEL` and `head` advances. Otherwise, the slot is updated to `remainingQueue`.

For simple withdrawals (`gasLimit == 0`), the portal transfers tokens directly to the recipient.

### Withdrawal Callbacks

For withdrawals with `gasLimit > 0`, the portal delegates to the `ZoneMessenger`. The messenger calls `transferFrom` to move tokens from the portal to the recipient, then calls the recipient with the provided `callbackData`. Both operations are atomic: if the callback reverts, the transfer reverts too.

Receiving contracts must implement `IWithdrawalReceiver` and return `onWithdrawalReceived.selector` to confirm successful handling. Receivers authenticate the call by checking `msg.sender == messenger`.

This enables composable withdrawals where funds flow directly into Tempo contracts (DEX swaps, staking, cross-zone deposits).

### Withdrawal Failures and Bounce-Back

> Related TIP: the guaranteed-liveness property of the zone-side refund mint described below relies on [TIP-1052: System-Contract Transfer Policy Exemption](https://github.com/tempoxyz/tempo/blob/main/tips/tip-1052.md), which introduces `ITIP20.systemForceMint` on the zone-side TIP-20 precompile and admits `ZoneInbox` to the zone-side Transfer Policy Exemption List.

Withdrawals can fail on the Tempo side for several reasons:

- TIP-403 policy restricts the portal or `withdrawal.to`
- The token is paused
- The callback reverts (out of gas, logic error)
- The receiver returns the wrong selector

To make sure that all of these cases can be handled without loss of user funds, every withdrawal carries a `fallbackRecipient`: a zone address that receives a refund mint if Tempo-side processing fails.

**Validation at withdrawal request time.** `requestWithdrawal(...)` requires `fallbackRecipient != address(0)` and reverts otherwise (`InvalidFallbackRecipient`). The address must also be authorized by the token's current TIP-403 policy as a mint recipient on the zone (reverts with `WithdrawalPolicyForbids` otherwise).

Checking the TIP-403 policy at request time guarantees that a later refund mint on the zone will not itself revert on policy grounds. The check uses the zone's view of the policy at the time of the request; later policy changes do not invalidate already-initiated withdrawals.

**Triggering conditions.** When `ZonePortal.processWithdrawal` runs on Tempo and the user-facing transfer or callback fails, the portal calls `_enqueueWithdrawalBounceBack(token, amount, fallbackRecipient)`. This constructs an internal `Deposit` with `to = fallbackRecipient`, `bouncebackRecipient = address(0)` (the sentinel reserved for portal-internal bounce-backs; see [Deposit Failures and Bounce-Back](#deposit-failures-and-bounce-back) — **No recursive bounces**), and appends it to the deposit queue:

```
currentDepositQueueHash = keccak256(abi.encode(DepositType.Regular, bounceBackDeposit, currentDepositQueueHash))
```

**Zone-side handling.** The next time the sequencer calls `ZoneInbox.advanceTempo`, the inbox sees a `Regular` deposit with `bouncebackRecipient == address(0)` and treats it as a terminal one-shot withdrawal-bounce-back. To preserve queue liveness against TIP-403 policy drift between request time and bounce-back processing time, the inbox uses `IZoneToken.systemForceMint(fallbackRecipient, amount)` instead of the ordinary `mint(...)`. `systemForceMint` skips the TIP-403 mint-recipient check while still enforcing pause and zero-recipient checks. `ZoneInbox` is an entry on the zone-side Transfer Policy Exemption List defined by [TIP-1052](https://github.com/tempoxyz/tempo/blob/main/tips/tip-1052.md), so this call is gated to the inbox predeploy alone. The `rejected` flag has no effect on this path: an internal withdrawal-bounce-back deposit cannot be rejected by the sequencer (the `rejected` flag is silently ignored, see [Sequencer rejection](#sequencer-rejection)).

The sequencer keeps the withdrawal fee regardless of whether the withdrawal succeeded on Tempo or bounced back.

**No recursive bounces.** The withdrawal-bounce-back path is one-shot by construction: the synthesized `Deposit` carries `bouncebackRecipient == address(0)`, which is the sentinel that disables the deposit-side bounce-back path on the zone. Combined with `systemForceMint` skipping the policy check, the zone-side mint is guaranteed to succeed, so no second bounce can be triggered.

### Authenticated Withdrawals

Zone transactions are private, but when a withdrawal is processed on Tempo, the `Withdrawal` struct is passed in calldata and publicly visible. To avoid leaking the sender's identity, the `sender` field is replaced with a `senderTag` commitment:

```
senderTag = keccak256(abi.encodePacked(sender, txHash))
```

The `txHash` is the hash of the `requestWithdrawal` transaction on the zone. Since zone transaction data is not published, `txHash` acts as a blinding factor known only to the sender and the sequencer.

The sender can optionally specify a `revealTo` public key (compressed secp256k1, 33 bytes) when requesting the withdrawal. If provided, the sequencer encrypts `(sender, txHash)` to that key using ECDH and populates `encryptedSender` in the withdrawal struct. The wire format is `ephemeralPubKey (33 bytes) || nonce (12 bytes) || ciphertext (52 bytes) || tag (16 bytes)` totaling 113 bytes.

Two disclosure modes are available:

- **Manual reveal**: The sender shares `txHash` with a verifier off-chain. The verifier checks `keccak256(abi.encodePacked(sender, txHash)) == senderTag`.
- **Encrypted reveal**: The holder of the `revealTo` private key decrypts `encryptedSender` to obtain `(sender, txHash)` and verifies against `senderTag`. No off-chain communication needed.

The sequencer computes `senderTag` and `encryptedSender` during `finalizeWithdrawalBatch`. This is trusted: a malicious sequencer could insert incorrect values. This is a modest extension of the existing trust model, where the sequencer is already trusted for liveness and transaction ordering.

For callback withdrawals, `IWithdrawalReceiver.onWithdrawalReceived` receives `bytes32 senderTag` instead of a plaintext sender address.

### Zone-to-Zone Transfers

Zones do not interoperate directly. Zone-to-zone transfers work through composable withdrawals on Tempo.

The sender on Zone A requests a withdrawal with `revealTo` set to Zone B's sequencer public key and `callbackData` that deposits into Zone B's portal. The flow:

1. Zone A processes the withdrawal and submits the batch to Tempo.
2. `processWithdrawal` on Tempo transfers tokens to Zone B's portal via the messenger callback.
3. Zone B's sequencer observes the incoming deposit and decrypts `encryptedSender` to learn `(sender, txHash)`.
4. Zone B verifies `keccak256(sender || txHash) == senderTag`, enabling sender-aware processing.

Sequencer encryption keys are already published (used for encrypted deposits), so no additional infrastructure is needed. This pattern generalizes beyond zone-to-zone: a withdrawal can swap on a Tempo DEX and deposit into another zone in a single composable flow.

<br>

## Zone Execution

### Fee Accounting

Zone transactions specify which enabled TIP-20 token to use for gas fees via a `feeToken` field. The sequencer accepts all enabled tokens as gas. Transactions use Tempo transaction semantics for fee payer, max fee per gas, and gas limit.

### Block Structure

Each zone block contains system transactions and user transactions in a fixed order:

1. `ZoneInbox.advanceTempo(header, deposits, decryptions, enabledTokens)` (optional, at the start of the block). Advances the zone's view of Tempo, enables newly-bridged tokens, processes any pending deposits, and verifies encrypted deposit decryptions. If omitted, the zone's Tempo binding carries forward from the previous block.
2. User transactions, executed in order.
3. `ZoneOutbox.finalizeWithdrawalBatch(count, blockNumber, encryptedSenders)` (required in the final block of a batch, absent in intermediate blocks). Constructs the withdrawal hash chain from pending withdrawals, populates `encryptedSender` for authenticated withdrawals, and writes the `withdrawalQueueHash` and `withdrawalBatchIndex` to state. Must be called even if there are zero withdrawals so the batch index advances.

A batch covers one or more zone blocks, with each batch interval targeting 250 milliseconds. The sequencer controls batch frequency, and intermediate blocks within a batch contain only `advanceTempo` (optional) and user transactions.

### Block Header Format

Zone blocks use a simplified header with fewer fields than a standard Ethereum header:

| Field | Type | Description |
|-------|------|-------------|
| `parentHash` | `bytes32` | Hash of the parent block header |
| `beneficiary` | `address` | Sequencer address (must match the registered sequencer) |
| `stateRoot` | `bytes32` | MPT root of the zone state after executing all transactions |
| `transactionsRoot` | `bytes32` | Root computed over the ordered list of block transactions |
| `receiptsRoot` | `bytes32` | Root computed over the ordered list of transaction receipts |
| `number` | `uint64` | Block number |
| `timestamp` | `uint64` | Block timestamp (must be non-decreasing) |
| `protocolVersion` | `uint64` | Zone protocol version |

The block hash is `keccak256` of the RLP-encoded header. Batch proofs commit to block hash transitions (`prevBlockHash` to `nextBlockHash`), not raw state roots, so the proof covers the full header structure.

### Privacy Modifications

Zone execution differs from standard Tempo execution in three areas. These changes are enforced at the EVM level, not just at the RPC layer, so they apply to all code paths including user transactions, `eth_call` simulations, and prover re-execution.

- **Balance and allowance access control.** `balanceOf(account)` reverts unless `msg.sender` is the account owner or the sequencer. `allowance(owner, spender)` reverts unless `msg.sender` is the owner, the spender, or the sequencer.
- **Fixed gas for transfers.** All TIP-20 transfer and approve operations charge a fixed 100,000 gas regardless of storage layout. This eliminates a side channel where variable gas costs reveal whether a recipient has previously received tokens.
- **Contract creation disabled.** `CREATE` and `CREATE2` revert. The zone runs only predeploys and TIP-20 token precompiles. Arbitrary contract deployment would allow users to circumvent the execution-level privacy controls.

<br>

## Tempo State Reads

The zone reads all of its configuration from Tempo: the sequencer address, the token registry, the deposit queue hash, and TIP-403 policy state. Everything flows through the `TempoState` predeploy.

### TempoState Predeploy

`TempoState` is deployed at `0x1c00000000000000000000000000000000000000`. It stores finalized Tempo block header fields and provides storage read access to Tempo contracts.

The predeploy exposes Tempo wrapper fields (`generalGasLimit`, `sharedGasLimit`) and selected inner Ethereum header fields (`parentHash`, `beneficiary`, `stateRoot`, `blockNumber`, `timestamp`, etc.). The `tempoBlockHash` is always `keccak256(RLP(TempoHeader))`, committing to the complete header contents even though only a subset of fields are stored.

Tempo headers are RLP-encoded as `rlp([general_gas_limit, shared_gas_limit, timestamp_millis_part, inner])`, where `inner` is a standard Ethereum header.

### Header Finalization

`ZoneInbox.advanceTempo()` calls `TempoState.finalizeTempo(header)` to advance the zone's view of Tempo. This function decodes the RLP header, validates chain continuity (parent hash must match the previous finalized header, block number must increment by one), and stores the header fields.

If a block omits `advanceTempo`, the Tempo binding carries forward from the previous block. Multiple blocks can share the same Tempo binding.

### Storage Reads

`TempoState` provides `readTempoStorageSlot(account, slot)` for reading storage from any Tempo contract. This function is restricted to zone system contracts (`ZoneInbox`, `ZoneOutbox`, `ZoneConfig`). User transactions cannot call it.

The function is a precompile stub. The actual storage reads are performed by the zone node and validated against the `tempoStateRoot` from the finalized header. The prover includes Merkle proofs for each unique account and storage slot accessed by system contracts during the batch.

Current callers:

- `ZoneInbox`: `currentDepositQueueHash` and encryption keys from the portal
- `ZoneConfig`: sequencer address, token registry from the portal

TIP-403 policy authorization on the zone is handled by a dedicated read-only proxy precompile (at the same address as the L1 `TIP403Registry`), which resolves policy queries via the zone node's policy provider rather than calling `readTempoStorageSlot` directly.

### Staleness and Finality

The zone's view of Tempo is only as current as the most recent `advanceTempo` call. If the sequencer advances Tempo infrequently, zone-side reads of portal state (sequencer address, deposit queue, token registry) may lag behind Tempo.

The zone node must only finalize Tempo headers that have reached finality on Tempo. Proofs should only reference finalized Tempo blocks to avoid reorg risk.

<br>

## TIP-403 Policies

Zones inherit compliance policies from Tempo automatically. Token issuers set transfer policies once on Tempo, and zones enforce them without any additional configuration.

### Policy Enforcement on Zones

The zone has a `TIP403Registry` deployed at the same address as on Tempo. This contract is read-only and does not support writing policies. Its `isAuthorized` function reads policy state from Tempo via `TempoState.readTempoStorageSlot()`.

Zone-side TIP-20 transfers check `isAuthorized(policyId, from)` and `isAuthorized(policyId, to)` before executing. If either check fails, the transfer reverts.

### Policy Inheritance

Issuers manage policies exclusively on Tempo. When an issuer freezes an address, updates a blacklist, or modifies a whitelist on Tempo, the zone inherits the change the next time `advanceTempo` imports a Tempo block containing the update.

If a TIP-403 policy restricts the portal address or a withdrawal recipient, the withdrawal fails on Tempo and bounces back to the sender's `fallbackRecipient` on the zone (see [Withdrawal Failures and Bounce-Back](#withdrawal-failures-and-bounce-back)).

<br>

## Private RPC

Zones expose a modified Ethereum JSON-RPC where every request is authenticated and every response is scoped to the caller's account. The RPC is the primary user interface and the main attack surface for privacy leaks.

### Authorization Tokens

Every RPC request must include an authorization token in the `X-Authorization-Token` HTTP header. The token proves the caller controls a Tempo account and scopes all responses to that account.

The signed message is `keccak256` of a packed encoding containing a `"TempoZoneRPC"` magic prefix, a version byte (currently `0`), the `zoneId`, `chainId`, `issuedAt`, and `expiresAt` timestamps. The wire format concatenates the signature and the 29-byte token fields, with the token fields always at the end.

A `zoneId` of `0` indicates an unscoped token valid for any zone. Zone IDs start at 1, so `0` is never a valid zone ID. The maximum validity window is 30 days (`expiresAt - issuedAt <= 2592000`). A clock skew tolerance of 60 seconds is allowed for `issuedAt`.

The RPC server rejects authorization tokens where:

- `zoneId` does not match the zone's configured `zoneId` and is not `0`.
- `chainId` does not match the zone's chain ID.
- `expiresAt - issuedAt > 2592000`.
- `expiresAt <= now`.
- `issuedAt > now + 60`.
- The signature is malformed or does not verify.
- For Keychain signatures: the signing key is not authorized, revoked, or expired in the zone's `AccountKeychain`.

Requests without an authorization token receive HTTP `401`. Requests with an invalid or expired token receive HTTP `403`.

### Signature Types

Authorization token signatures follow the same format as Tempo transaction signatures:

| Type | Detection | Authentication |
|------|-----------|----------------|
| secp256k1 | 65 bytes, no prefix | Standard `ecrecover` |
| P256 | Prefix `0x01`, 130 bytes | Public key embedded in signature |
| WebAuthn | Prefix `0x02`, variable length | P256 key via WebAuthn assertion |
| Keychain V1 | Prefix `0x03` | Wraps inner sig + `user_address`, authenticates as root account |
| Keychain V2 | Prefix `0x04` | Same as V1 but binds `user_address` into signing hash |

Keychain keys allow session keys and scoped access keys to authenticate to the RPC with the same permissions as the root account. The zone has its own independent `AccountKeychain` instance, not mirrored from Tempo. Users must register keychain keys on the zone directly.

### Method Access Control

The RPC uses a default-deny model. Any method not explicitly listed returns `-32601` (method not found). Methods fall into four categories:

**Allowed.** `eth_chainId`, `eth_blockNumber`, `eth_gasPrice`, `eth_maxPriorityFeePerGas`, `eth_feeHistory`, `eth_getBlockByNumber` and `eth_getBlockByHash` (without full transactions), `eth_syncing`, `eth_coinbase`, `net_version`, `net_listening`, `web3_clientVersion`, `web3_sha3`.

**Scoped.** Available to any authenticated caller but filtered to the caller's account:

- `eth_getBalance`, `eth_getTransactionCount`: return `0x0` for non-self queries (no error, to avoid leaking account existence).
- `eth_getTransactionByHash`, `eth_getTransactionReceipt`: return `null` if the caller is not the sender.
- `eth_sendRawTransaction`: rejects if the transaction sender does not match the authenticated account.
- `eth_call`, `eth_estimateGas`: `from` must equal the authenticated account. State override sets and block override objects are rejected for non-sequencer callers.
- `eth_getLogs`, `eth_getFilterLogs`, `eth_getFilterChanges`: filtered to TIP-20 events where the caller is a relevant party (see [Event Filtering](#event-filtering)).
- `eth_newFilter`, `eth_newBlockFilter`, `eth_uninstallFilter`: allowed, filters are scoped to the authenticated account.

**Restricted (sequencer-only).** Methods that expose raw state, full block data, or transaction-level detail that would break per-account privacy. This includes raw state access (`eth_getStorageAt`, `eth_getCode`, `eth_createAccessList`), full block queries (`eth_getBlockByNumber`/`eth_getBlockByHash` with full transactions, `eth_getBlockReceipts`, `eth_getBlockTransactionCountByNumber`/`Hash`, `eth_getTransactionByBlockNumberAndIndex`/`HashAndIndex`, `eth_getUncleCountByBlockNumber`/`Hash`), and all `debug_*`, `admin_*`, and `txpool_*` namespace methods.

**Disabled.** Methods not available on zones. `eth_getProof` leaks trie structure. `eth_newPendingTransactionFilter` and `eth_subscribe("newPendingTransactions")` enable mempool observation. Uncle query methods (`eth_getUncleByBlockNumberAndIndex`, `eth_getUncleByBlockHashAndIndex`) and mining methods (`eth_mining`, `eth_hashrate`, `eth_getWork`, `eth_submitWork`, `eth_submitHashrate`) do not apply to zones.

### Block Responses

For non-sequencer callers, block responses are modified:

- The `transactions` field is always an empty array, regardless of the `include_transactions` parameter.
- The `logsBloom` field is zeroed. The Bloom filter summarizes all log topics and emitting addresses in the block, so returning the real value would allow probing whether a specific address had activity in that block.
- All other header fields (`number`, `hash`, `parentHash`, `timestamp`, `stateRoot`, `gasUsed`, etc.) are returned normally. Aggregate activity metrics are intentionally public.

The sequencer receives full block data.

### Event Filtering

All log queries are restricted to TIP-20 events where the authenticated account is a relevant party:

| Event | Relevant if |
|-------|-------------|
| `Transfer(from, to, amount)` | `from == caller` OR `to == caller` |
| `Approval(owner, spender, amount)` | `owner == caller` OR `spender == caller` |
| `TransferWithMemo(from, to, amount, memo)` | `from == caller` OR `to == caller` |
| `Mint(to, amount)` | `to == caller` |
| `Burn(from, amount)` | `from == caller` |

All other events (system events, configuration events) are filtered out. The `address` filter parameter must be a zone token address or omitted. The RPC server injects topic filters to restrict indexed address parameters to the caller, then post-filters results as a final pass.

### Timing Side Channels

Scoped methods that fetch data before checking authorization leak existence via timing differences. The RPC server enforces a minimum response time of 100ms on `eth_getTransactionByHash`, `eth_getTransactionReceipt`, `eth_getLogs`, `eth_getFilterLogs`, and `eth_getFilterChanges`.

Methods where authorization is checked before any data fetch (`eth_getBalance`, `eth_call`, `eth_sendRawTransaction`) do not need the speed bump.

### WebSocket Subscriptions

WebSocket connections follow the same authorization model. The authorization token is provided during the handshake and scopes all subscriptions for that connection.

- `eth_subscribe("newHeads")`: allowed, pushes block headers with `logsBloom` zeroed for non-sequencer callers.
- `eth_subscribe("logs")`: scoped to the authenticated account using the same event filtering rules.
- `eth_subscribe("newPendingTransactions")`: disabled.

The connection is terminated when the authorization token expires. For keychain-authenticated connections, the server must also terminate the connection within 1 second of importing a block that revokes the keychain key.

### Zone-Specific Methods

The zone exposes three methods under the `zone_` namespace:

| Method | Access | Description |
|--------|--------|-------------|
| `zone_getAuthorizationTokenInfo` | Any authenticated | Returns the authenticated account address and token expiry |
| `zone_getZoneInfo` | Any authenticated | Returns `zoneId`, `zoneTokens`, `sequencer`, `chainId` |
| `zone_getDepositStatus(tempoBlockNumber)` | Scoped | Returns deposit processing status for the given Tempo block, filtered to deposits where the caller is the sender or recipient |

There are no state-changing methods via authorization token. Withdrawals require a signed transaction submitted via `eth_sendRawTransaction`.

### Error Codes

| Code | Message | When |
|------|---------|------|
| `-32001` | Authorization token required | No token provided |
| `-32002` | Authorization token expired | Token has expired |
| `-32003` | Transaction rejected | Sender mismatch on `eth_sendRawTransaction` |
| `-32004` | Account mismatch | `from` mismatch on `eth_call` / `eth_estimateGas` |
| `-32005` | Sequencer only | Method requires sequencer access |
| `-32006` | Method disabled | Method not available on zones |

Methods where the user explicitly supplies a mismatched parameter return explicit errors (the user already knows the address they provided). Methods that query about other accounts return silent dummy values (`0x0`, `null`, empty results) to avoid revealing "data exists but you can't see it."

<br>

## Proving System

The proving system is proof-agnostic. The core is a pure state transition function that takes a witness, executes zone blocks, and outputs commitments for onchain verification. The onchain verifier is abstracted behind `IVerifier`, and the portal does not care how the proof was produced. Any proving backend (ZKVM, TEE, or otherwise) can run the same state transition function.

### State Transition Function

The entry point is a pure function:

```rust
pub fn prove_zone_batch(witness: BatchWitness) -> Result<BatchOutput, Error>
```

It takes a complete witness of zone blocks and their dependencies, executes EVM state transitions (including system transactions), and outputs commitments for onchain verification. The core commitment is the zone block hash transition, not the raw state root. The function is `no_std` compatible for portability across proving backends.

### Witness Structure

The witness contains everything needed to re-execute the batch:

- **PublicInputs**: `prev_block_hash`, `tempo_block_number`, `anchor_block_number`, `anchor_block_hash`, `expected_withdrawal_batch_index`, `sequencer`. These are the values the portal passes to the verifier and the proof must be consistent with.
- **BatchWitness**: the public inputs, the previous batch's block header, the zone blocks to execute, the initial zone state, Tempo state proofs, and Tempo ancestry headers (for ancestry validation).
- **ZoneBlock**: `number`, `parent_hash`, `timestamp`, `beneficiary`, `protocol_version`, `tempo_header_rlp` (optional), `deposits`, `decryptions`, `finalize_withdrawal_batch_count` (optional), and user `transactions`.
- **ZoneStateWitness**: the initial zone state root, a deduplicated pool of zone-state trie nodes, and decoded account / storage reads needed to bootstrap execution. Only accounts and storage slots accessed during execution are included. Missing witness data must produce an error, not default to zero, to prevent the prover from omitting non-zero state.

### Input Schematic

The prover inputs are nested containers. `BatchWitness` is the top-level object passed into `prove_zone_batch`, and the schematic below shows one representative entry for repeated collections such as `ZoneBlock[i]`, `QueuedDeposit[j]`, `ZoneAccountRead[k]`, `ZoneStorageRead[k]`, and `L1StateRead[k]`. To keep the picture readable, the boxes list field names rather than repeating every Rust scalar type.

```mermaid
flowchart TB
    subgraph BW["BatchWitness"]
        direction TB

        PI["PublicInputs<br/>prev_block_hash<br/>tempo_block_number<br/>anchor_block_number<br/>anchor_block_hash<br/>expected_withdrawal_batch_index<br/>sequencer"]

        PH["ZoneHeader<br/>parent_hash<br/>beneficiary<br/>state_root<br/>transactions_root<br/>receipts_root<br/>number<br/>timestamp<br/>protocol_version"]

        subgraph ZBL["zone_blocks"]
            direction TB
            ZB["ZoneBlock[i]<br/>number<br/>parent_hash<br/>timestamp<br/>beneficiary<br/>protocol_version<br/>tempo_header_rlp<br/>finalize_withdrawal_batch_count<br/>transactions"]

            subgraph DEP["deposits"]
                direction TB
                QD["QueuedDeposit[j]<br/>deposit_type<br/>deposit_data"]

                subgraph PAYLOAD["deposit_data payload"]
                    direction TB
                    D["Deposit<br/>token<br/>sender<br/>to<br/>amount<br/>memo"]

                    ED["EncryptedDeposit<br/>token<br/>sender<br/>amount<br/>keyIndex<br/>encrypted"]

                    EDP["EncryptedDepositPayload<br/>ephemeralPubkeyX<br/>ephemeralPubkeyYParity<br/>ciphertext<br/>nonce<br/>tag"]

                    D ~~~ ED
                    ED ~~~ EDP
                end

                QD ~~~ D
            end

            subgraph DEC["decryptions"]
                direction TB
                DD["DecryptionData[k]<br/>shared_secret<br/>shared_secret_y_parity<br/>to<br/>memo<br/>cp_proof"]
                CP["ChaumPedersenProof<br/>s<br/>c"]
                DD ~~~ CP
            end

            ZB ~~~ QD
            QD ~~~ DD
        end

        subgraph ZSW["initial_zone_state"]
            direction TB
            ZSWBOX["ZoneStateWitness<br/>state_root<br/>node_pool"]
            ZAR["ZoneAccountRead[k]<br/>account<br/>nonce<br/>balance<br/>code_hash<br/>code"]
            ZSR["ZoneStorageRead[k]<br/>account<br/>slot<br/>value"]
            ZSWBOX ~~~ ZAR
            ZAR ~~~ ZSR
        end

        subgraph BSP["tempo_state_proofs"]
            direction TB
            BSPBOX["BatchStateProof<br/>node_pool"]
            READ["L1StateRead[k]<br/>zone_block_index<br/>tempo_block_number<br/>account<br/>slot<br/>value"]
            BSPBOX ~~~ READ
        end

        AH["tempo_ancestry_headers<br/>header bytes [0..n]"]

        PI ~~~ PH
        PH ~~~ ZB
        ZB ~~~ ZSWBOX
        ZSWBOX ~~~ BSPBOX
        BSPBOX ~~~ AH
    end
```

### Detailed Input Definitions

The prover-side inputs are defined concretely below. Types that mirror the onchain ABI (`QueuedDeposit`, `DecryptionData`, `ChaumPedersenProof`) keep the same field ordering and semantics as the interface definitions in [Common Types](#common-types).

```rust
pub struct PublicInputs {
    /// Previous batch's block hash (must equal portal.blockHash)
    pub prev_block_hash: B256,

    /// Tempo block number for the batch (must equal portal's tempoBlockNumber)
    pub tempo_block_number: u64,

    /// Anchor Tempo block number (tempo_block_number or recent block in EIP-2935 window)
    pub anchor_block_number: u64,

    /// Anchor Tempo block hash (must equal portal's EIP-2935 lookup)
    pub anchor_block_hash: B256,

    /// Expected withdrawal batch index (passed by portal as withdrawalBatchIndex + 1)
    pub expected_withdrawal_batch_index: u64,

    /// Registered sequencer (passed by portal; zone block beneficiary must match)
    pub sequencer: Address,
}

pub struct BatchWitness {
    /// Public inputs committed by the proof system
    pub public_inputs: PublicInputs,

    /// Previous batch's block header (for state-root binding)
    pub prev_block_header: ZoneHeader,

    /// Zone blocks to execute
    pub zone_blocks: Vec<ZoneBlock>,

    /// Initial zone state
    pub initial_zone_state: ZoneStateWitness,

    /// Tempo state proofs for Tempo reads
    pub tempo_state_proofs: BatchStateProof,

    /// Tempo headers for ancestry verification (only in ancestry mode)
    /// Ordered from tempo_block_number + 1 to anchor_block_number.
    pub tempo_ancestry_headers: Vec<Vec<u8>>,
}

pub struct ZoneHeader {
    pub parent_hash: B256,
    pub beneficiary: Address,
    pub state_root: B256,
    pub transactions_root: B256,
    pub receipts_root: B256,
    pub number: u64,
    pub timestamp: u64,
    pub protocol_version: u64,
}

pub struct ZoneBlock {
    /// Block number
    pub number: u64,

    /// Parent block hash
    pub parent_hash: B256,

    /// Timestamp
    pub timestamp: u64,

    /// Beneficiary (must match registered sequencer)
    pub beneficiary: Address,

    /// Protocol version encoded into the zone block header
    pub protocol_version: u64,

    /// Tempo header RLP used by the call (ZoneInbox.advanceTempo).
    /// If None, the block does not advance Tempo and the binding carries over.
    pub tempo_header_rlp: Option<Vec<u8>>,

    /// Deposits processed by the system tx (oldest first, unified queue).
    /// Must be empty if tempo_header_rlp is None.
    pub deposits: Vec<QueuedDeposit>,

    /// Decryption data for encrypted deposits in the system tx.
    /// Must be empty if tempo_header_rlp is None.
    pub decryptions: Vec<DecryptionData>,

    /// Sequencer-only: finalize a batch (only in final block, must be last)
    /// Required for the final block in a batch; must be absent in intermediate blocks.
    /// Uses U256 to match Solidity `finalizeWithdrawalBatch(uint256 count)`.
    pub finalize_withdrawal_batch_count: Option<U256>,

    /// Transactions to execute
    pub transactions: Vec<Transaction>,
}

/// Mirrors the Solidity `QueuedDeposit` struct from IZone.sol
pub struct QueuedDeposit {
    pub deposit_type: DepositType,
    pub deposit_data: Vec<u8>, // abi.encode(Deposit) or abi.encode(EncryptedDeposit)
}

pub enum DepositType {
    Regular,
    Encrypted,
}

/// Mirrors the Solidity `DecryptionData` struct from IZone.sol
/// Provided by the sequencer for each encrypted deposit
pub struct DecryptionData {
    pub shared_secret: B256,        // ECDH shared secret (x-coordinate)
    pub shared_secret_y_parity: u8, // Y coordinate parity of the shared secret point
    pub to: Address,                // Decrypted recipient
    pub memo: B256,                 // Decrypted memo
    pub cp_proof: ChaumPedersenProof,
}

pub struct ChaumPedersenProof {
    pub s: B256, // Response: s = r + c * privSeq (mod n)
    pub c: B256, // Challenge: c = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)
}

pub struct ZoneStateWitness {
    /// Zone state root at start of batch
    pub state_root: B256,

    /// Deduplicated pool of all zone-state MPT nodes
    pub node_pool: HashMap<B256, Vec<u8>>,

    /// Decoded account leaves needed to bootstrap execution
    pub account_reads: Vec<ZoneAccountRead>,

    /// Decoded storage leaves needed to bootstrap execution
    pub storage_reads: Vec<ZoneStorageRead>,
}

pub struct ZoneAccountRead {
    pub account: Address,
    pub nonce: u64,
    pub balance: U256,
    pub code_hash: B256,
    pub code: Option<Vec<u8>>,
}

pub struct ZoneStorageRead {
    pub account: Address,
    pub slot: U256,
    pub value: U256,
}

pub struct BatchStateProof {
    /// Deduplicated pool of all MPT nodes
    pub node_pool: HashMap<B256, Vec<u8>>,

    /// Tempo state reads verified against the shared node pool
    pub reads: Vec<L1StateRead>,
}

pub struct L1StateRead {
    /// Which zone block performed this read
    pub zone_block_index: u64,

    /// Which Tempo block to read from (must match TempoState for this block)
    pub tempo_block_number: u64,

    /// Tempo account and storage slot
    pub account: Address,
    pub slot: U256,

    /// Expected value
    pub value: U256,
}
```

### Shared Trie Proof Format

`ZoneStateWitness` and `BatchStateProof` both use the same trie-proof encoding:

- `node_pool` is a deduplicated map from `keccak256(rlp(node))` to the node's raw RLP bytes. The prover validates each node once by recomputing the hash.
- Each read descriptor (`ZoneAccountRead`, `ZoneStorageRead`, or `L1StateRead`) states which decoded account or storage value must be proven against a bound trie root.
- Verification walks the account trie using `keccak256(account)` and, when needed, the storage trie using `keccak256(slot)`, fetching branch, extension, and leaf nodes from `node_pool`.
- For `ZoneAccountRead`, the account leaf proves the committed `code_hash`, but not the bytecode preimage itself. If the witness supplies `code`, the prover must additionally require `keccak256(code) == code_hash` before materializing that account into the execution state.
- Missing leaves are represented by valid non-membership proofs. An absent account is interpreted as the canonical empty account: `nonce = 0`, `balance = 0`, `code = None`, `code_hash = KECCAK_EMPTY`, and an empty storage trie. An absent storage leaf is interpreted as zero.
- Client databases may still retain historical trie nodes that are no longer reachable from the current root, but those stale nodes are irrelevant to proof verification because only nodes reachable from the bound root contribute to the proof.

`ZoneStateWitness` applies this shared trie proof format to the initial zone-state root at batch start. `account_reads` and `storage_reads` describe the decoded account and storage values needed to bootstrap execution. To initialize execution, the prover checks that `ZoneStateWitness.state_root` is consistent with `prev_block_header.state_root`, validates `node_pool`, proves each `ZoneAccountRead` and `ZoneStorageRead` against that initial root, checks `keccak256(code) == code_hash` for every supplied account-code preimage, materializes the resulting account and storage values into the execution state, and only then starts replaying blocks. Missing account or storage reads are errors; they must not silently default to zero.

### Batch Output

The state transition function produces:

| Field | Description |
|-------|-------------|
| `block_transition` | `prev_block_hash` to `next_block_hash` covering all blocks in the batch |
| `deposit_queue_transition` | `prev_processed_hash` to `next_processed_hash` for deposit processing |
| `withdrawal_queue_hash` | Hash chain of withdrawals finalized in this batch (`0` if none) |
| `last_batch_commitment` | `withdrawal_batch_index` read from `ZoneOutbox.lastBatch` |

### Block Execution (Stateless prover execution function)

The stateless execution function must reject the witness on any failed check, missing read, or inconsistent state transition. A correct implementation proceeds in the following order:

1. **Bind the previous block header to the public inputs.**
   Require `keccak256(rlp(prev_block_header)) == public_inputs.prev_block_hash`. Require `prev_block_header.state_root == initial_zone_state.state_root`. These checks ensure that the witness starts from the exact predecessor block already committed on Tempo.

2. **Verify and materialize the initial zone state.**
   Apply the [shared trie proof format](#shared-trie-proof-format) to `initial_zone_state`: validate every node in `initial_zone_state.node_pool`, prove each `ZoneAccountRead` and `ZoneStorageRead` against `initial_zone_state.state_root`, require `keccak256(code) == code_hash` for every supplied account-code preimage, interpret non-membership as the canonical empty account or zero storage, and load the decoded results into the prover's in-memory execution state. After this step, ordinary zone-state reads during execution come from the materialized state, not from repeated Merkle-proof checks.

3. **Verify and index the Tempo proof pool.**
   Validate every node in `tempo_state_proofs.node_pool` once by recomputing `keccak256(rlp(node))` for each node.

4. **For each `zone_blocks[i]`, verify the block witness before executing it.**
   Require `block.parent_hash == prev_block_hash`. Require `block.number == prev_header.number + 1`. Require `block.timestamp >= prev_header.timestamp`. Require `block.beneficiary == public_inputs.sequencer`. Require `finalize_withdrawal_batch_count` to be absent in intermediate blocks and present in the final block of the batch. If `tempo_header_rlp` is absent, require `deposits` and `decryptions` to be empty.

5. **Execute `advanceTempo` if the block imports a Tempo header.**
   If `tempo_header_rlp` is present, call `TempoState.finalizeTempo(header)` in the modeled execution environment. This validates header continuity, updates the bound `tempoBlockNumber`, `tempoBlockHash`, and `tempoStateRoot`, and make the new Tempo root available for subsequent `TempoState.readTempoStorageSlot` calls in this block. Require the finalized `tempoBlockHash` to equal `keccak256(tempo_header_rlp)`.

6. **Process deposits and encrypted deposit decryptions inside `advanceTempo`.**
   Using the now-bound Tempo root for this block, verify the Tempo-side reads needed by `ZoneInbox` such as the portal's current deposit queue hash. Process the `deposits` in witness order, enforcing the queue semantics specified in [Deposit Queue](#deposit-queue). For encrypted deposits, verify the supplied `DecryptionData` and Chaum-Pedersen proof, decode the recipient and memo when valid, and apply the fallback mint-to-sender path when decryption verification fails as specified in [Onchain Decryption Verification](#onchain-decryption-verification).

7. **Execute user transactions in order.**
   Run each user transaction against the materialized zone state using the current block environment. Whenever execution calls `TempoState.readTempoStorageSlot`, satisfy that call by locating the corresponding `L1StateRead`, proving it against the Tempo root currently bound for this block, and requiring the decoded value to match the witness entry. Any zone-state or Tempo-state access not covered by the witness is an error.

8. **Execute `finalizeWithdrawalBatch` at the end of the final block.**
   If `finalize_withdrawal_batch_count` is present, execute `ZoneOutbox.finalizeWithdrawalBatch(count)` after all user transactions in that block. This must update the outbox's last-batch state and compute the `withdrawal_queue_hash` committed by the batch. Intermediate blocks must not execute this call.

9. **Compute the resulting block header and carry it forward.**
    After block execution, compute the `transactionsRoot` and `receiptsRoot` over the full ordered list of transactions and receipts for that block. Construct the simplified `ZoneHeader` from `parent_hash`, `beneficiary`, `state_root`, `transactions_root`, `receipts_root`, `number`, `timestamp`, and `protocol_version`, then compute `next_block_hash = keccak256(rlp(header))`. Set `prev_block_hash = next_block_hash` and `prev_header = header` before moving to the next block.

10. **Extract the final batch commitments from the post-state.**
    Read the final `ZoneInbox.processedDepositQueueHash`, `ZoneOutbox.lastBatch`, `TempoState.tempoBlockNumber`, and `TempoState.tempoBlockHash` from the executed state.

11. **Verify the batch's final Tempo binding and anchor.**
    Require `TempoState.tempoBlockNumber == public_inputs.tempo_block_number`. If `anchor_block_number == tempo_block_number`, require `TempoState.tempoBlockHash == anchor_block_hash`. Otherwise, verify the parent-hash chain from `tempo_block_number` to `anchor_block_number` using `tempo_ancestry_headers`, ending at `anchor_block_hash`.

12. **Return the batch outputs.**
    Set `block_transition.prev_block_hash = public_inputs.prev_block_hash` and `block_transition.next_block_hash = prev_block_hash` after the final block. Set `deposit_queue_transition.prev_processed_hash` to the value captured in step 4 and `deposit_queue_transition.next_processed_hash` to the final inbox processed hash. Set `withdrawal_queue_hash` and `last_batch_commitment.withdrawal_batch_index` from the final `ZoneOutbox.lastBatch` state.

### Tempo State Proofs

System contracts read Tempo state during execution (deposit queue hash, sequencer address, token registry, TIP-403 policies). `BatchStateProof` applies the [shared trie proof format](#shared-trie-proof-format) to the Tempo root currently bound in `TempoState` at the moment of each read. If `advanceTempo()` runs during the batch, later reads are therefore verified against the newer Tempo root, not the root from the start of the batch. The witness includes a `BatchStateProof` containing:

- A deduplicated `node_pool` of MPT nodes, keyed by `keccak256(rlp(node))`. Each node is verified exactly once.
- A list of `L1StateRead` entries, each specifying the zone block index, Tempo block number, account, storage slot, and expected value.

Reads are indexed and verified on demand during execution. Each `L1StateRead` is additionally tagged with `zone_block_index` and `tempo_block_number` so the prover can bind that read to the correct in-batch `TempoState`. The proof shape is the same as `ZoneStateWitness`; the difference is timing. `ZoneStateWitness` is verified once against the initial zone-state root at batch start, while `BatchStateProof` reads are verified against the Tempo root currently bound in `TempoState` at the moment of each read.

Anchor validation ensures the zone's view of Tempo is correct. If `anchor_block_number` equals `tempo_block_number`, the zone's `tempoBlockHash` must match `anchor_block_hash` directly. If `anchor_block_number` is greater (for zones that have been offline longer than the EIP-2935 window), the proof verifies the parent-hash chain from `tempo_block_number` to `anchor_block_number` using the ancestry headers in the witness.

### Deployment Modes

The state transition function runs in any backend that can execute the `no_std` Rust function. Examples include ZKVMs and TEE environments. The same `prove_zone_batch` function is used regardless of backend.

<br>

## Batch Submission

The sequencer submits batches to Tempo via `ZonePortal.submitBatch()`. Each batch covers one or more zone blocks and includes a proof that the state transition was executed correctly.

### submitBatch

The call takes the following parameters:

| Parameter | Description |
|-----------|-------------|
| `tempoBlockNumber` | The Tempo block the zone committed to via `TempoState` |
| `recentTempoBlockNumber` | A recent Tempo block for ancestry validation (`0` for direct lookup) |
| `blockTransition` | Zone block hash transition: `prevBlockHash` to `nextBlockHash` |
| `depositQueueTransition` | Deposit queue processing: `prevProcessedHash` to `nextProcessedHash` |
| `withdrawalQueueHash` | Hash chain of withdrawals finalized in this batch (`0` if none) |
| `verifierConfig` | Opaque payload for the verifier (domain separation, attestation data) |
| `proof` | The proof or attestation produced by the proving backend |

On success, the portal:

1. Updates `blockHash` to `nextBlockHash`.
2. Updates `lastSyncedTempoBlockNumber` to `tempoBlockNumber`.
3. Advances `withdrawalBatchIndex`.
4. Adds the withdrawal hash chain to the next slot in the withdrawal queue ring buffer (if `withdrawalQueueHash` is non-zero).
5. Emits `BatchSubmitted`.

### Verifier Interface

The portal calls the verifier to validate each batch:

```solidity
interface IVerifier {
    function verify(
        uint64 tempoBlockNumber,
        uint64 anchorBlockNumber,
        bytes32 anchorBlockHash,
        uint64 expectedWithdrawalBatchIndex,
        address sequencer,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        bytes32 withdrawalQueueHash,
        bytes calldata verifierConfig,
        bytes calldata proof
    ) external view returns (bool);
}
```

The portal computes `anchorBlockNumber` and `anchorBlockHash` from the submission parameters (see [Anchor Block Validation](#anchor-block-validation)) and passes them alongside the portal's current `withdrawalBatchIndex + 1` as `expectedWithdrawalBatchIndex` and the registered `sequencer` address. The `verifierConfig` and `proof` are opaque to the portal.

### Anchor Block Validation

The portal needs to verify that the zone's view of Tempo (via `TempoState`) is anchored to a real Tempo block. It looks up a block hash via the EIP-2935 block hash history precompile and passes it to the verifier.

If `recentTempoBlockNumber` is `0`, the portal looks up `tempoBlockNumber` directly from EIP-2935. The proof must show that the zone's `tempoBlockHash` matches this hash.

If `recentTempoBlockNumber` is greater than `tempoBlockNumber`, the portal looks up `recentTempoBlockNumber` from EIP-2935 instead. The proof verifies the parent-hash chain from `tempoBlockNumber` to `recentTempoBlockNumber` internally, using Tempo headers included in the witness. This allows batch submission even when `tempoBlockNumber` has rotated out of the EIP-2935 window (roughly 8192 blocks), preventing the zone from being bricked after extended downtime.

`recentTempoBlockNumber` must be strictly greater than `tempoBlockNumber` when non-zero. Both values must be at or after `genesisTempoBlockNumber`.

### Proof Requirements

The proof must validate:

1. The state transition from `prevBlockHash` to `nextBlockHash` is correct.
2. The zone committed to `tempoBlockNumber` via `TempoState`.
3. The zone's `tempoBlockHash` matches `anchorBlockHash` (direct), or the parent-hash chain from `tempoBlockNumber` to `anchorBlockNumber` is valid (ancestry).
4. `ZoneOutbox.lastBatch().withdrawalBatchIndex` equals `expectedWithdrawalBatchIndex`.
5. `ZoneOutbox.lastBatch().withdrawalQueueHash` matches the submitted `withdrawalQueueHash`.
6. Every zone block `beneficiary` matches `sequencer`.
7. Deposit processing is correct (the zone read `currentDepositQueueHash` from Tempo state and processed deposits accordingly).

## Zone Precompiles

Zones have three categories of precompiles: TIP-20 token precompiles (one per enabled token) and two cryptographic precompiles for encrypted deposit verification.

### TIP-20 Token Precompile

Each enabled TIP-20 token is deployed as a precompile at the same address as on Tempo. The precompile implements the standard TIP-20 interface with privacy modifications:

- `balanceOf` and `allowance` are restricted to the account owner (or sequencer).
- Transfer-family operations (`transfer`, `transferFrom`, `approve`) charge a fixed 100,000 gas.
- `mint` is restricted to `ZoneInbox`, `burn` is restricted to `ZoneOutbox`.

### Chaum-Pedersen Verify

| | |
|---|---|
| **Address** | `0x1c00000000000000000000000000000000000100` |
| **Gas** | ~8,000 |

```solidity
interface IChaumPedersenVerify {
    function verifyProof(
        bytes32 ephemeralPubX,
        uint8 ephemeralPubYParity,
        bytes32 sharedSecret,
        uint8 sharedSecretYParity,
        bytes32 sequencerPubX,
        uint8 sequencerPubYParity,
        ChaumPedersenProof calldata proof
    ) external view returns (bool valid);
}
```

Verifies that an ECDH shared secret was correctly derived from the sequencer's private key and an ephemeral public key, without exposing the private key. Used during [onchain decryption verification](#onchain-decryption-verification) of encrypted deposits.

The verifier reconstructs commitments `R1 = s*G - c*pubSeq` and `R2 = s*ephemeralPub - c*sharedSecretPoint`, recomputes the Fiat-Shamir challenge `c' = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)`, and checks `c == c'`.

### AES-GCM Decrypt

| | |
|---|---|
| **Address** | `0x1c00000000000000000000000000000000000101` |
| **Gas** | ~1,000 base + ~500 per 32 bytes of ciphertext |

```solidity
interface IAesGcmDecrypt {
    function decrypt(
        bytes32 key,
        bytes12 nonce,
        bytes calldata ciphertext,
        bytes calldata aad,
        bytes16 tag
    ) external view returns (bytes memory plaintext, bool valid);
}
```

Performs AES-256-GCM decryption and authentication tag verification. Returns the decrypted plaintext and `true` if the tag validates, or empty bytes and `false` otherwise. Used during [onchain decryption verification](#onchain-decryption-verification) of encrypted deposits.

HKDF-SHA256 key derivation (used to derive the AES key from the ECDH shared secret) is implemented in Solidity using the SHA256 precompile at `0x02`, keeping this precompile minimal.

<br>

## Contracts and Interfaces

This section lists the key types and contract interfaces referenced throughout the spec. Only the essential functions are shown. Implementations may include additional view functions and events.

### Common Types

```solidity
struct Deposit {
    address token;
    address sender;
    address to;
    uint128 amount;
    bytes32 memo;
}

struct Withdrawal {
    address token;
    bytes32 senderTag;          // keccak256(abi.encodePacked(sender, txHash))
    address to;
    uint128 amount;
    uint128 fee;
    bytes32 memo;
    uint64 gasLimit;
    address fallbackRecipient;
    bytes callbackData;         // max 1KB
    bytes encryptedSender;      // ECDH-encrypted (sender, txHash), or empty
}

struct EncryptedDeposit {
    address token;
    address sender;
    uint128 amount;
    uint256 keyIndex;
    EncryptedDepositPayload encrypted;
}

struct EncryptedDepositPayload {
    bytes32 ephemeralPubkeyX;
    uint8 ephemeralPubkeyYParity;
    bytes ciphertext;
    bytes12 nonce;
    bytes16 tag;
}

enum DepositType {
    Regular,
    Encrypted
}

struct QueuedDeposit {
    DepositType depositType;
    bytes depositData;  // abi.encode(Deposit) or abi.encode(EncryptedDeposit)
}

struct DecryptionData {
    bytes32 sharedSecret;
    uint8 sharedSecretYParity;
    address to;
    bytes32 memo;
    ChaumPedersenProof cpProof;
}

struct ChaumPedersenProof {
    bytes32 s;  // response
    bytes32 c;  // challenge
}

struct BlockTransition {
    bytes32 prevBlockHash;
    bytes32 nextBlockHash;
}

struct DepositQueueTransition {
    bytes32 prevProcessedHash;
    bytes32 nextProcessedHash;
}

struct TokenConfig {
    bool enabled;
    bool depositsActive;
}

struct ZoneInfo {
    uint32 zoneId;
    address portal;
    address messenger;
    address initialToken;
    address sequencer;
    address verifier;
    bytes32 genesisBlockHash;
    bytes32 genesisTempoBlockHash;
    uint64 genesisTempoBlockNumber;
}

struct ZoneParams {
    bytes32 genesisBlockHash;
    bytes32 genesisTempoBlockHash;
    uint64 genesisTempoBlockNumber;
}

struct LastBatch {
    bytes32 withdrawalQueueHash;
    uint64 withdrawalBatchIndex;
}
```

### IZoneFactory

```solidity
interface IZoneFactory {
    struct CreateZoneParams {
        address initialToken;
        address sequencer;
        address verifier;
        ZoneParams zoneParams;
    }

    event ZoneCreated(
        uint32 indexed zoneId, address indexed portal, address indexed messenger,
        address initialToken, address sequencer, address verifier,
        bytes32 genesisBlockHash, bytes32 genesisTempoBlockHash, uint64 genesisTempoBlockNumber
    );

    function createZone(CreateZoneParams calldata params) external returns (uint32 zoneId, address portal);
    function zoneCount() external view returns (uint32);
    function zones(uint32 zoneId) external view returns (ZoneInfo memory);
    function isZonePortal(address portal) external view returns (bool);
}
```

### IZonePortal

```solidity
interface IZonePortal {
    // Events
    event DepositMade(
        bytes32 indexed newCurrentDepositQueueHash, address indexed sender,
        address token, address to, uint128 netAmount, uint128 fee, uint128 bouncebackFee, bytes32 memo,
        address bouncebackRecipient, uint64 depositNumber
    );
    event EncryptedDepositMade(
        bytes32 indexed newCurrentDepositQueueHash, address indexed sender,
        address token, uint128 netAmount, uint128 fee, uint128 bouncebackFee, uint256 keyIndex,
        bytes32 ephemeralPubkeyX, uint8 ephemeralPubkeyYParity,
        bytes ciphertext, bytes12 nonce, bytes16 tag, uint64 depositNumber
    );
    event BatchSubmitted(
        uint64 indexed withdrawalBatchIndex, bytes32 nextProcessedDepositQueueHash,
        bytes32 nextBlockHash, bytes32 withdrawalQueueHash,
        uint64 lastProcessedDepositNumber
    );
    event WithdrawalProcessed(address indexed to, address token, uint128 amount, bool callbackSuccess);
    event WithdrawalBounceBack(
        bytes32 indexed newCurrentDepositQueueHash, address indexed fallbackRecipient,
        address token, uint128 amount, uint64 depositNumber
    );
    event DepositBounceBack(
        address indexed bouncebackRecipient, address token,
        uint128 amount, uint128 bouncebackFee
    );
    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);
    event SequencerEncryptionKeyUpdated(bytes32 x, uint8 yParity, uint256 keyIndex, uint64 activationBlock);
    event ZoneGasRateUpdated(uint128 zoneGasRate);
    event TokenEnabled(address indexed token, string name, string symbol, string currency);
    event DepositsPaused(address indexed token);
    event DepositsResumed(address indexed token);

    // Token management
    function enableToken(address token) external;
    function pauseDeposits(address token) external;
    function resumeDeposits(address token) external;
    function isTokenEnabled(address token) external view returns (bool);
    function areDepositsActive(address token) external view returns (bool);
    function enabledTokenCount() external view returns (uint256);
    function enabledTokenAt(uint256 index) external view returns (address);

    // Deposits
    /// @dev Reverts (`MissingBouncebackRecipient`) if `bouncebackRecipient == address(0)`,
    ///      and validates the recipient against the token's current TIP-403 policy. Every
    ///      user-initiated deposit must carry a usable refund target so that a failed mint
    ///      can be recovered without stalling the deposit queue.
    function deposit(
        address token, address to, uint128 amount, bytes32 memo, address bouncebackRecipient
    ) external returns (bytes32 newCurrentDepositQueueHash);
    /// @dev Reverts (`MissingBouncebackRecipient`) if `bouncebackRecipient == address(0)`,
    ///      and validates the recipient against the token's current TIP-403 policy. A
    ///      ciphertext that fails onchain decryption verification has no well-defined
    ///      recipient on the zone, so the bounce-back target must always be set.
    function depositEncrypted(
        address token, uint128 amount, uint256 keyIndex,
        EncryptedDepositPayload calldata encrypted, address bouncebackRecipient
    ) external returns (bytes32 newCurrentDepositQueueHash);
    function calculateDepositFee() external view returns (uint128 fee);
    function depositCount() external view returns (uint64);
    function lastProcessedDepositNumber() external view returns (uint64);

    // Batch submission
    function submitBatch(
        uint64 tempoBlockNumber, uint64 recentTempoBlockNumber,
        BlockTransition calldata blockTransition, DepositQueueTransition calldata depositQueueTransition,
        bytes32 withdrawalQueueHash, bytes calldata verifierConfig, bytes calldata proof
    ) external;

    // Withdrawal processing
    function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external;

    // Sequencer management
    function transferSequencer(address newSequencer) external;
    function acceptSequencer() external;
    function setZoneGasRate(uint128 _zoneGasRate) external;

    // Encryption keys
    function setSequencerEncryptionKey(bytes32 x, uint8 yParity, uint8 popV, bytes32 popR, bytes32 popS) external;
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);
    function isEncryptionKeyValid(uint256 keyIndex) external view returns (bool valid, uint64 expiresAtBlock);

    // State
    function sequencer() external view returns (address);
    function verifier() external view returns (address);
    function blockHash() external view returns (bytes32);
    function currentDepositQueueHash() external view returns (bytes32);
    function withdrawalBatchIndex() external view returns (uint64);
    function lastSyncedTempoBlockNumber() external view returns (uint64);
}
```

### IZoneMessenger

```solidity
interface IZoneMessenger {
    function portal() external view returns (address);
    function relayMessage(
        address token, bytes32 senderTag, address target,
        uint128 amount, uint64 gasLimit, bytes calldata data
    ) external;
}
```

### IWithdrawalReceiver

```solidity
interface IWithdrawalReceiver {
    function onWithdrawalReceived(
        bytes32 senderTag, address token, uint128 amount, bytes calldata callbackData
    ) external returns (bytes4);
}
```

The receiver must return `IWithdrawalReceiver.onWithdrawalReceived.selector` to confirm successful handling.

### ITempoState

Address: `0x1c00000000000000000000000000000000000000`

```solidity
interface ITempoState {
    event TempoBlockFinalized(bytes32 indexed blockHash, uint64 indexed blockNumber, bytes32 stateRoot);

    function tempoBlockHash() external view returns (bytes32);
    function tempoBlockNumber() external view returns (uint64);
    function tempoStateRoot() external view returns (bytes32);
    function tempoTimestamp() external view returns (uint64);

    function finalizeTempo(bytes calldata header) external;
    function readTempoStorageSlot(address account, bytes32 slot) external view returns (bytes32);
    function readTempoStorageSlots(address account, bytes32[] calldata slots) external view returns (bytes32[] memory);
}
```

### IZoneInbox

Address: `0x1c00000000000000000000000000000000000001`

```solidity
interface IZoneInbox {
    /// @notice A deposit queued for processing on the zone, paired with the
    ///         sequencer's accept/reject decision.
    /// @dev `rejected` is supplied by the sequencer per-deposit when calling
    ///      advanceTempo. It is NOT part of the deposit queue hash chain (which
    ///      stays canonical to the deposit content produced by the portal). A
    ///      rejected user-initiated deposit skips the zone-side mint (and, for
    ///      encrypted deposits, the onchain decryption verification) and bounces
    ///      back to bouncebackRecipient on Tempo. A `rejected = true` flag on an
    ///      internal withdrawal-bounce-back deposit (bouncebackRecipient ==
    ///      address(0)) is silently ignored.
    struct QueuedDeposit {
        DepositType depositType;
        bytes depositData; // abi.encode(Deposit) or abi.encode(EncryptedDeposit)
        bool rejected;
    }

    event TempoAdvanced(
        bytes32 indexed tempoBlockHash, uint64 indexed tempoBlockNumber,
        uint256 depositsProcessed, bytes32 newProcessedDepositQueueHash
    );
    event DepositProcessed(
        bytes32 indexed depositHash, address indexed sender, address indexed to,
        address token, uint128 amount, bytes32 memo
    );
    event EncryptedDepositProcessed(
        bytes32 indexed depositHash, address indexed sender, address indexed to,
        address token, uint128 amount, bytes32 memo
    );
    event EncryptedDepositFailed(
        bytes32 indexed depositHash, address indexed sender, address token, uint128 amount
    );
    /// @notice Emitted when the sequencer marks a deposit as rejected via QueuedDeposit.rejected.
    /// @dev Distinguishes operator-initiated rejection from a TIP-403 / mint failure
    ///      (DepositFailed) or an invalid-encryption / decrypted-mint failure
    ///      (EncryptedDepositFailed). Funds are bounced back identically.
    event DepositRejected(
        bytes32 indexed depositHash,
        address indexed sender,
        DepositType depositType,
        address token,
        uint128 amount,
        address bouncebackRecipient
    );
    event TokenEnabled(address indexed token, string name, string symbol, string currency);

    function processedDepositQueueHash() external view returns (bytes32);
    function advanceTempo(
        bytes calldata header, QueuedDeposit[] calldata deposits, DecryptionData[] calldata decryptions,
        EnabledToken[] calldata enabledTokens
    ) external;
}
```

`EnabledToken` carries token metadata (`token`, `name`, `symbol`, `currency`) for provisioning zone-side TIP-20 precompiles via the TIP-20 factory.

### IZoneOutbox

Address: `0x1c00000000000000000000000000000000000002`

```solidity
interface IZoneOutbox {
    event WithdrawalRequested(
        uint64 indexed withdrawalIndex, address indexed sender, address token, address to,
        uint128 amount, uint128 fee, bytes32 memo, uint64 gasLimit,
        address fallbackRecipient, bytes data, bytes revealTo
    );
    event TempoGasRateUpdated(uint128 tempoGasRate);
    event MaxWithdrawalsPerBlockUpdated(uint256 maxWithdrawalsPerBlock);
    event BatchFinalized(bytes32 indexed withdrawalQueueHash, uint64 withdrawalBatchIndex);

    function tempoGasRate() external view returns (uint128);
    function lastBatch() external view returns (LastBatch memory);
    function calculateWithdrawalFee(uint64 gasLimit) external view returns (uint128);
    function setTempoGasRate(uint128 _tempoGasRate) external;

    function requestWithdrawal(
        address token, address to, uint128 amount, bytes32 memo,
        uint64 gasLimit, address fallbackRecipient, bytes calldata data
    ) external;

    function requestWithdrawal(
        address token, address to, uint128 amount, bytes32 memo,
        uint64 gasLimit, address fallbackRecipient, bytes calldata data, bytes calldata revealTo
    ) external;

    function finalizeWithdrawalBatch(uint256 count, uint64 blockNumber, bytes[] calldata encryptedSenders)
        external returns (bytes32 withdrawalQueueHash);
}
```

### IZoneConfig

Address: `0x1c00000000000000000000000000000000000003`

```solidity
interface IZoneConfig {
    function sequencer() external view returns (address);
    function isSequencer(address account) external view returns (bool);
    function isEnabledToken(address token) external view returns (bool);
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);
}
```

Reads the sequencer address, token registry, and encryption key from the portal on Tempo via `TempoState` storage reads.

### TIP-403 Registry

Deployed at the same address as on Tempo. Read-only on the zone. Its `isAuthorized(policyId, account)` function reads policy state from Tempo via `TempoState.readTempoStorageSlot()`. Zone-side TIP-20 transfers call this automatically.

<br>

## Network Upgrades and Hard Fork Activation

> **Note:** The verifier rotation and protocol version mechanisms described below are the target design. The current `ZonePortal` implementation declares `verifier` as `immutable`, so the rotation mechanism is not yet implemented. This section will be updated when the upgrade contracts are deployed.

Zones activate hard fork upgrades in lockstep with Tempo using same-block activation. The trigger is the Tempo block number: the zone block whose `advanceTempo` imports the fork Tempo block uses the new execution rules for its entire scope.

The portal will maintain two verifier slots (`verifier` and `forkVerifier`). At each fork, verifiers rotate: the previous fork verifier is promoted to `verifier`, and the new fork verifier takes the `forkVerifier` slot. At most two verifiers are active at any time. The `IVerifier` interface is unchanged across forks. New proof parameters are passed via the opaque `verifierConfig` bytes.

`ZoneFactory` will maintain a `protocolVersion` counter incremented at each fork. Zone nodes embed the highest protocol version they support and halt cleanly if the imported Tempo block bumps `protocolVersion` beyond their supported version, preventing an outdated node from producing blocks under incorrect rules.

No onchain action is required from zone operators. The new verifier is deployed and rotated as part of the Tempo hard fork. Operators upgrade their zone node binary and prover program before the fork. When the fork Tempo block arrives, the node activates new rules automatically.

The portal will enforce a `forkActivationBlock` cutoff where batches targeting the old `verifier` must have `tempoBlockNumber < forkActivationBlock`. This prevents post-fork batches from being submitted against old verification rules.

The Tempo hard fork block executes the following as system transactions:

1. Deploy the new `IVerifier` contract.
2. Call `ZoneFactory.setForkVerifier(forkVerifier)`, which for each registered portal promotes `forkVerifier` to `verifier`, installs the new verifier as `forkVerifier`, and sets `forkActivationBlock = block.number`.
3. Increment `ZoneFactory.protocolVersion`.

If the fork changes zone predeploy behavior, the zone node injects new bytecode at the predeploy addresses before `advanceTempo` executes in the first post-fork zone block.

The two-verifier invariant means only the two most recent verifiers are active at any time. A zone that falls more than one full fork cycle behind loses the ability to submit its oldest unproven batches once the N-2 verifier is deprecated.

If the operator does not upgrade before the fork, the zone node detects the unsupported protocol version and halts cleanly. If the node is upgraded but the prover is stale, zone execution continues but settlement pauses until the new prover is installed. In both cases, user funds remain safe in the portal.
