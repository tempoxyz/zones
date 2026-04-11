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
    - [Batch Output](#batch-output)
    - [Block Execution](#block-execution)
    - [Tempo State Proofs](#tempo-state-proofs)
    - [Deployment Modes](#deployment-modes)
  - [Batch Submission](#batch-submission)
    - [submitBatch](#submitbatch)
    - [Verifier Interface](#verifier-interface)
    - [Anchor Block Validation](#anchor-block-validation)
    - [Proof Requirements](#proof-requirements)
  - [Withdrawals](#withdrawals)
    - [Withdrawal Request](#withdrawal-request)
    - [Withdrawal Fees](#withdrawal-fees)
    - [Withdrawal Batching](#withdrawal-batching)
    - [Withdrawal Queue](#withdrawal-queue)
    - [Withdrawal Processing](#withdrawal-processing)
    - [Callback Withdrawals](#callback-withdrawals)
    - [Withdrawal Failures and Bounce-Back](#withdrawal-failures-and-bounce-back)
    - [Authenticated Withdrawals](#authenticated-withdrawals)
    - [Zone-to-Zone Transfers](#zone-to-zone-transfers)
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
- [Security Considerations](#security-considerations)
- [Open Questions](#open-questions)

---

# Abstract

A Tempo Zone is a private execution environment anchored to Tempo. Inside a zone, balances, transfers, and transaction history are invisible to block explorers, indexers, and other users. Each zone is operated by a dedicated sequencer that is the sole block producer, settling back to Tempo through a proof-agnostic verification system.

Funds enter a zone through deposits on Tempo, where they are held in escrow. The zone mints equivalent tokens, and users transact privately with balances and transaction history hidden behind authenticated RPC access and execution-level controls. When users withdraw, tokens are burned on the zone and released from escrow on Tempo. Proofs guarantee that the sequencer executed every transaction correctly and cannot forge state transitions. Withdrawals support optional callbacks, making them composable with Tempo contracts and enabling zone-to-zone transfers.

This document specifies the zone protocol: deployment, sequencer operations, deposits, execution, the private RPC interface, the proving system, batch submission, withdrawals, precompiles, contract interfaces, and the network upgrade process.

# Specification

## Terminology

| Term | Definition |
|------|------------|
| Tempo | The base chain that zones settle to. |
| Zone | A private execution environment anchored to Tempo. |
| Portal | The contract on Tempo that escrows deposited tokens and finalizes withdrawals for a zone. |
| Batch | A sequencer-produced commitment covering one or more zone blocks, submitted to Tempo with a proof. |
| Enabled token | A TIP-20 token that the sequencer has activated for deposits and withdrawals on a zone. Enablement is permanent. |
| TIP-20 | Tempo's fungible token standard. |
| TIP-403 | Tempo's compliance registry. Issuers attach transfer policies (whitelists, blacklists) to TIP-20 tokens. |
| Predeploy | A system contract deployed at a fixed address on the zone at genesis. |

<br>

## System Overview

Each zone is operated by a **sequencer** that collects transactions, produces blocks, generates proofs, and submits batches to Tempo. A single registered address controls sequencer operations for each zone. **Users** deposit TIP-20 tokens from Tempo into the zone, transact privately, and withdraw back to Tempo.

On the Tempo side, an onchain **verifier** contract validates that each batch was executed correctly. The verifier is abstracted behind a minimal interface (`IVerifier`) and is proof-agnostic. Any proving backend (ZK, TEE, or otherwise) can implement the interface. The portal does not care how the proof was produced.

On Tempo, each zone has a **portal** that escrows deposited tokens. When a user deposits, the portal locks their tokens and appends the deposit to a queue. The sequencer observes the deposit, advances the zone's view of Tempo, and mints equivalent tokens on the zone.

Users transact on the zone privately. Balances, transfers, and transaction history are only visible to the account holder and the sequencer.

When a user wants to exit, they request a withdrawal on the zone. Their tokens are burned, and the withdrawal is added to a pending list. At the end of a batch, the sequencer finalizes all pending withdrawals into a hash chain and generates a proof covering the full batch of zone blocks. The sequencer submits this batch and proof to the portal on Tempo, which verifies the proof and queues the withdrawals. The sequencer then processes each withdrawal, releasing tokens from escrow to the recipient.

```mermaid
sequenceDiagram
    participant User
    participant Portal as Tempo (Portal)
    participant Zone as Zone (Sequencer)

    User->>Portal: deposit(token, to, amount)
    Portal->>Portal: escrow tokens, append to deposit queue
    Zone->>Portal: observe deposit
    Zone->>Zone: advanceTempo(), mint tokens to recipient

    User->>Zone: transact privately

    User->>Zone: requestWithdrawal()
    Zone->>Zone: burn tokens, add to pending withdrawals
    Zone->>Zone: finalizeWithdrawalBatch()
    Zone->>Zone: generate proof

    Zone->>Portal: submitBatch(proof)
    Portal->>Portal: verify proof, queue withdrawals

    Portal->>User: processWithdrawal(), tokens released from escrow
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
chain_id = 4217000000 + zone_id
```

The prefix `4217` is derived from the Tempo chain ID. This ensures replay protection between zones. A transaction signed for one zone cannot be replayed on another. The chain ID is set in the zone's genesis configuration and validated by the zone node at startup.

### Tempo Contracts

A single [`ZoneFactory`](#izonefactory) on Tempo creates zones and maintains the registry of all deployed zones. When a zone is created, the factory deploys two contracts for it:

| Contract | Purpose |
|----------|---------|
| [`ZonePortal`](#izoneportal) | Escrows deposited tokens, accepts batch submissions, verifies proofs, and processes withdrawals. Manages the token registry and deposit/withdrawal queues. |
| [`ZoneMessenger`](#izonemessenger) | Relays withdrawal callbacks. When a withdrawal includes calldata, the messenger transfers tokens from the portal to the recipient and executes the callback atomically. Deployed separately from the portal to isolate callback execution. |

The portal gives the messenger max approval for each enabled token so that withdrawal callbacks can transfer tokens from escrow to the recipient in a single call.

### Zone Predeploys

Each zone has four system contracts deployed at genesis at fixed addresses:

| Predeploy | Address | Purpose |
|-----------|---------|---------|
| [`TempoState`](#itempostate) | `0x1c00...0000` | Stores finalized Tempo block headers and provides storage read access to Tempo contracts. |
| [`ZoneInbox`](#izoneinbox) | `0x1c00...0001` | Advances the zone's view of Tempo and processes incoming deposits. Sole mint authority. |
| [`ZoneOutbox`](#izoneoutbox) | `0x1c00...0002` | Handles withdrawal requests and batch finalization. Sole burn authority. |
| [`ZoneConfig`](#izoneconfig) | `0x1c00...0003` | Central configuration. Reads the sequencer address and token registry from Tempo via `TempoState`. |

`ZoneConfig` reads the sequencer address and token registry from the portal on Tempo via `TempoState` storage reads, making Tempo the single source of truth for zone configuration. See [Tempo L1 State Reads](#tempo-l1-state-reads) for details.

### Zone Token Model

Zones have no TIP-20 factory and contract creation is disabled (`CREATE` and `CREATE2` revert). All TIP-20 tokens on a zone are representations of Tempo tokens, deployed at the same address as on Tempo. When the sequencer enables a token on the portal, the zone node provisions a TIP-20 precompile at that address.

Token supply on the zone is controlled exclusively by the system contracts:

- `ZoneInbox` mints tokens when processing deposits from Tempo.
- `ZoneOutbox` burns tokens when users request withdrawals.

The zone-side supply of each token always equals net deposits minus net withdrawals. The corresponding tokens on Tempo are held in escrow by the portal. No other actor can mint or burn zone tokens.

<br>

## Sequencer Operations

### Token Management

The sequencer manages which TIP-20 tokens are available on the zone:

- `enableToken(token)`: Enable a new TIP-20 for deposits and withdrawals. This is **irreversible**. Once enabled, a token can never be disabled.
- `pauseDeposits(token)`: Pause new deposits for a token. Does not affect withdrawals.
- `resumeDeposits(token)`: Resume deposits for a previously paused token.

The portal maintains a `TokenConfig` per token with an `enabled` flag (permanent) and a `depositsActive` flag (toggleable), along with an append-only `enabledTokens` list. This enforces the non-custodial withdrawal guarantee: the sequencer can halt deposits but can never prevent users from withdrawing an enabled token.

### Gas Rate Configuration

The sequencer configures two gas rates that determine fees for deposits and withdrawals:

| Rate | Set via | Used for |
|------|---------|----------|
| `zoneGasRate` | `ZonePortal.setZoneGasRate()` | Deposit fees: `FIXED_DEPOSIT_GAS (100,000) * zoneGasRate` |
| `tempoGasRate` | `ZoneOutbox.setTempoGasRate()` | Withdrawal fees: `gasLimit * tempoGasRate` |

Both rates are denominated in token units per gas unit. A single uniform `zoneGasRate` applies to all tokens. Fees are paid in the same token being deposited or withdrawn.

The sequencer takes the risk on Tempo gas price fluctuations for withdrawals. If actual gas costs on Tempo exceed the fee collected, the sequencer covers the difference. If costs are lower, the sequencer keeps the surplus.

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

Deposits move TIP-20 tokens from Tempo into a zone. The user deposits on Tempo, the portal escrows the tokens and appends the deposit to a hash chain, and the sequencer mints equivalent tokens on the zone.

### Regular Deposits

A user deposits by calling `deposit(token, to, amount, memo)` on the portal. The portal:

1. Validates the token is enabled and deposits are active.
2. Transfers `amount` from the user into escrow.
3. Deducts the deposit fee (see [Deposit Fees](#deposit-fees)) and pays it to the sequencer immediately.
4. Appends the deposit to the deposit queue hash chain with the net amount (`amount - fee`).
5. Emits `DepositMade`.

The sequencer observes `DepositMade` events and relays deposits to the zone via `ZoneInbox.advanceTempo()`. This function processes deposits in order, minting the zone-side TIP-20 token to each recipient: `mint(deposit.to, deposit.amount)`.

Deposits always succeed on the zone. There are no callbacks or failure modes for regular deposits. If the sequencer withholds deposits, funds remain in escrow with no forced inclusion mechanism.

### Deposit Fees

Each deposit incurs a fixed processing fee:

```
fee = FIXED_DEPOSIT_GAS * zoneGasRate
    = 100,000 * zoneGasRate
```

The fee is paid in the same token being deposited. It is deducted from the deposit amount and paid to the sequencer immediately on Tempo. The deposit queue stores the net amount (`amount - fee`), which is what gets minted on the zone. A deposit must be large enough to cover the fee; otherwise the portal reverts with `DepositTooSmall`.

### Deposit Queue

Deposits flow from Tempo to the zone through a hash chain. The portal tracks a single `currentDepositQueueHash` representing the head of the chain. Each new deposit wraps the existing hash:

```
currentDepositQueueHash = keccak256(abi.encode(DepositType.Regular, deposit, currentDepositQueueHash))
```

The newest deposit is always outermost, making onchain addition O(1). The zone tracks its own `processedDepositQueueHash` in state. During `advanceTempo()`, the zone processes deposits oldest-first, rebuilding the hash chain and validating that the result matches `currentDepositQueueHash` read from Tempo state via `TempoState.readTempoStorageSlot()`.

After a batch is accepted, the portal updates `lastSyncedTempoBlockNumber` to record how far Tempo state was synced. Users can check whether their deposit has been processed by comparing their deposit's Tempo block number against this value.

### Encrypted Deposits

Users can encrypt the recipient and memo of a deposit so that only the sequencer can see who received the funds. The token, sender, and amount remain public (required for onchain escrow accounting), but the `to` address and `memo` are encrypted.

The encryption scheme is ECIES with secp256k1:

1. The user generates an ephemeral keypair and derives a shared secret via ECDH with the sequencer's published encryption key.
2. The user derives an AES-256 key from the shared secret using HKDF-SHA256.
3. The user encrypts `(to || memo || padding)` with AES-256-GCM, producing ciphertext, a nonce, and an authentication tag.
4. The user calls `depositEncrypted(token, amount, keyIndex, encryptedPayload)` on the portal, where `keyIndex` references which encryption key they encrypted to (see [Encryption Key Management](#encryption-key-management)).

The portal escrows the tokens, appends the encrypted deposit to the deposit queue, and emits `EncryptedDepositMade`. The sequencer decrypts the payload off-chain and provides the decrypted `(to, memo)` when processing the deposit on the zone via `advanceTempo()`.

Regular and encrypted deposits share a single ordered queue with a type discriminator in the hash:

```
keccak256(abi.encode(DepositType.Regular, deposit, prevHash))
keccak256(abi.encode(DepositType.Encrypted, encryptedDeposit, prevHash))
```

Deposits are processed in the exact order they were made, regardless of type.

| Field | Visibility | Reason |
|-------|------------|--------|
| `token` | Public | Required for onchain escrow accounting and zone-side minting |
| `sender` | Public | Required for refunds if decryption fails |
| `amount` | Public | Required for onchain accounting |
| `to` | Encrypted | Only the sequencer learns the recipient |
| `memo` | Encrypted | Only the sequencer learns the payment context |

### Onchain Decryption Verification

When the sequencer processes an encrypted deposit on the zone, it claims the ciphertext decrypts to a specific `(to, memo)`. The zone verifies this onchain without the sequencer revealing their private key.

The sequencer provides the ECDH shared secret alongside the decrypted data. Verification proceeds in three steps:

1. **Chaum-Pedersen proof.** The sequencer provides a zero-knowledge proof that the shared secret was correctly derived: "I know `privSeq` such that `pubSeq = privSeq * G` AND `sharedSecretPoint = privSeq * ephemeralPub`." The [Chaum-Pedersen Verify](#chaum-pedersen-verify) precompile checks this proof. The sequencer's public key is looked up from the onchain key history, not supplied by the sequencer, preventing key substitution.

2. **AES-GCM decryption.** The zone derives an AES-256 key from the shared secret using HKDF-SHA256 (implemented in Solidity using the SHA256 precompile at `0x02`). The HKDF info string includes `tempoPortal`, `keyIndex`, and `ephemeralPubkeyX` for domain separation. The [AES-GCM Decrypt](#aes-gcm-decrypt) precompile decrypts the ciphertext and validates the GCM authentication tag.

3. **Plaintext validation.** The zone confirms the decrypted plaintext matches the `(to, memo)` the sequencer claimed. The plaintext is packed as `[address (20 bytes)][memo (32 bytes)][padding (12 bytes)]` totaling 64 bytes.

If any step fails (invalid proof, GCM tag mismatch, plaintext mismatch), the zone mints the tokens to the sender's address on the zone instead. The Tempo-side funds remain in escrow. This ensures chain progress is never blocked by invalid encrypted deposits.

The Chaum-Pedersen proof also prevents griefing. Without it, a user could submit garbage ciphertext that the sequencer cannot decrypt and cannot prove invalid, blocking the chain. The proof lets the sequencer demonstrate correct shared secret derivation, and the GCM tag failure then proves the ciphertext itself was invalid.

<br>

## Zone Execution

### Fee Accounting

Zone transactions specify which enabled TIP-20 token to use for gas fees via a `feeToken` field. The sequencer accepts all enabled tokens as gas. Transactions use Tempo transaction semantics for fee payer, max fee per gas, and gas limit.

### Block Structure

Each zone block contains system transactions and user transactions in a fixed order:

1. `ZoneInbox.advanceTempo(header, deposits, decryptions)` (optional, at the start of the block). Advances the zone's view of Tempo, processes any pending deposits, and verifies encrypted deposit decryptions. If omitted, the zone's Tempo binding carries forward from the previous block.
2. User transactions, executed in order.
3. `ZoneOutbox.finalizeWithdrawalBatch(count)` (required in the final block of a batch, absent in intermediate blocks). Constructs the withdrawal hash chain from pending withdrawals and writes the `withdrawalQueueHash` and `withdrawalBatchIndex` to state. Must be called even if there are zero withdrawals so the batch index advances.

A batch covers one or more zone blocks. The sequencer controls batch frequency. Intermediate blocks within a batch contain only `advanceTempo` (optional) and user transactions.

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
| `timestamp` | `uint64` | Block timestamp (must be monotonically increasing) |
| `protocolVersion` | `uint64` | Zone protocol version |

The block hash is `keccak256` of the RLP-encoded header. Batch proofs commit to block hash transitions (`prevBlockHash` to `nextBlockHash`), not raw state roots, so the proof covers the full header structure.

### Privacy Modifications

Zone execution differs from standard Tempo execution in three areas. These changes are enforced at the EVM level, not just at the RPC layer, so they apply to all code paths including user transactions, `eth_call` simulations, and prover re-execution.

- **Balance and allowance access control.** `balanceOf(account)` reverts unless `msg.sender` is the account owner or the sequencer. `allowance(owner, spender)` reverts unless `msg.sender` is the owner, the spender, or the sequencer.
- **Fixed gas for transfers.** All TIP-20 transfer and approve operations charge a fixed 100,000 gas regardless of storage layout. This eliminates a side channel where variable gas costs reveal whether a recipient has previously received tokens.
- **Contract creation disabled.** `CREATE` and `CREATE2` revert. The zone runs only predeploys and TIP-20 token precompiles. Arbitrary contract deployment would allow users to circumvent the execution-level privacy controls.

See the [Execution Environment Specification](./execution.md) for full details.

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

`TempoState` provides `readTempoStorageSlot(account, slot)` for reading storage from any Tempo contract. This function is restricted to zone system contracts only: `ZoneInbox`, `ZoneOutbox`, and `ZoneConfig`. User transactions cannot call it.

The function is a precompile stub. The actual storage reads are performed by the zone node and validated against the `tempoStateRoot` from the finalized header. The prover includes Merkle proofs for each unique account and storage slot accessed by system contracts during the batch.

System contracts use storage reads for:

- `ZoneConfig`: sequencer address, token registry from the portal
- `ZoneInbox`: `currentDepositQueueHash` from the portal
- `TIP403Registry`: policy state from Tempo

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

Zones expose a modified Ethereum JSON-RPC where every request is authenticated and every response is scoped to the caller's account. The RPC is the primary user interface and the main attack surface for privacy leaks. This section summarizes the design; the full specification is in the [Zone RPC Specification](./rpc.md).

### Authorization Tokens

Every RPC request must include an authorization token in the `X-Authorization-Token` HTTP header. The token proves the caller controls a Tempo account and scopes all responses to that account.

The signed message is `keccak256` of a packed encoding containing a `"TempoZoneRPC"` magic prefix, a version byte (currently `0`), the `zoneId`, `chainId`, `issuedAt`, and `expiresAt` timestamps. The wire format concatenates the signature and the 29-byte token fields, with the token fields always at the end.

A `zoneId` of `0` indicates an unscoped token valid for any zone. Zone IDs start at 1, so `0` is never a valid zone ID. The maximum validity window is 30 days (`expiresAt - issuedAt <= 2592000`). A clock skew tolerance of 60 seconds is allowed for `issuedAt`.

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

**Allowed.** Public zone information available to any authenticated caller: `eth_chainId`, `eth_blockNumber`, `eth_gasPrice`, `eth_feeHistory`, `eth_getBlockByNumber` (without full transactions), `eth_syncing`, `net_version`, `web3_clientVersion`, and others.

**Scoped.** Available to any authenticated caller but filtered to the caller's account:

- `eth_getBalance`, `eth_getTransactionCount`: return `0x0` for non-self queries (no error, to avoid leaking account existence).
- `eth_getTransactionByHash`, `eth_getTransactionReceipt`: return `null` if the caller is not the sender.
- `eth_sendRawTransaction`: rejects if the transaction sender does not match the authenticated account.
- `eth_call`, `eth_estimateGas`: `from` must equal the authenticated account. State override sets and block override objects are rejected for non-sequencer callers.
- `eth_getLogs`, `eth_getFilterLogs`, `eth_getFilterChanges`: filtered to TIP-20 events where the caller is a relevant party (see [Event Filtering](#event-filtering)).

**Restricted (sequencer-only).** `eth_getStorageAt`, `eth_getCode`, `eth_getBlockByNumber` with full transactions, `eth_getBlockReceipts`, `debug_*`, `admin_*`, `txpool_*`, and other methods that expose raw state or full block data.

**Disabled.** `eth_getProof` (leaks trie structure), `eth_newPendingTransactionFilter` and `eth_subscribe("newPendingTransactions")` (mempool observation), and mining-related methods.

### Block Responses

For non-sequencer callers, block responses are modified:

- The `transactions` field is always an empty array, regardless of the `include_transactions` parameter.
- The `logsBloom` field is zeroed. The Bloom filter summarizes all log topics and emitting addresses in the block; returning the real value would allow probing whether a specific address had activity in that block.
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

See the [Zone RPC Specification](./rpc.md) for the complete method tables, wire format details, and security considerations.

<br>

## Proving System

<!-- The proving system is proof-agnostic. The core is a pure state transition function in Rust (no_std) that executes zone blocks and outputs commitments for onchain verification. Any proving backend can run this function. The onchain verifier is abstracted behind IVerifier and the portal does not care how the proof was produced. -->

### State Transition Function

<!--
- prove_zone_batch(witness: BatchWitness) -> Result<BatchOutput, Error>
- Pure function: takes witness, executes EVM transitions, outputs commitments
- Core commitment is zone block hash transition (not raw state root)
- no_std compatible for portability across proving backends
-->

### Witness Structure

<!--
- PublicInputs: prev_block_hash, tempo_block_number, anchor_block_number, anchor_block_hash, expected_withdrawal_batch_index, sequencer
- BatchWitness: public_inputs, prev_block_header, zone_blocks, initial_zone_state, tempo_state_proofs, tempo_ancestry_headers
- ZoneBlock: number, parent_hash, timestamp, beneficiary, tempo_header_rlp (optional), deposits, decryptions, finalize_withdrawal_batch_count (optional), transactions
- ZoneStateWitness: accounts with MPT proofs, state_root — only includes accounts/slots accessed during batch
- Missing witness data must error, not default to zero (prevents prover from omitting non-zero state)
-->

### Batch Output

<!--
- BlockTransition: prev_block_hash → next_block_hash
- DepositQueueTransition: prev_processed_hash → next_processed_hash
- withdrawal_queue_hash: hash chain for this batch (0 if none)
- LastBatchCommitment: withdrawal_batch_index from ZoneOutbox.lastBatch
-->

### Block Execution

<!--
- For each block: validate parent hash, block number, timestamp monotonicity, beneficiary == sequencer
- System tx: advanceTempo (optional, start of block) — processes deposits, validates Tempo header binding
- User txs: executed in order via revm
- System tx: finalizeWithdrawalBatch (required in final block only, absent in intermediate blocks)
- Block hash computed from simplified zone header (parentHash, beneficiary, stateRoot, transactionsRoot, receiptsRoot, number, timestamp, protocolVersion)
-->

### Tempo State Proofs

<!--
- BatchStateProof: deduplicated node_pool (MPT nodes) + L1StateRead list
- Each read specifies: zone_block_index, tempo_block_number, account, slot, node_path, expected value
- Verified once per proof, indexed for on-demand access during execution
- Anchor validation: direct (anchor == tempo block, hashes match) or ancestry (parent-hash chain verified inside proof)
-->

### Deployment Modes

<!--
- The state transition function is proof-agnostic and runs in any backend
- Examples: ZKVM, TEE, or any environment that can execute the no_std Rust function
- Same prove_zone_batch function regardless of backend
- Reference to prover-design.md for full implementation details
-->

<br>

## Batch Submission

### submitBatch

<!-- Parameters, what gets updated onchain -->

### Verifier Interface

<!-- IVerifier.verify() signature, what each parameter means -->

### Anchor Block Validation

<!-- EIP-2935 lookup, ancestry chain for historical blocks, when each is used -->

### Proof Requirements

<!-- Enumerated list of everything the proof must validate -->

<br>

## Withdrawals

### Withdrawal Request

<!-- User approves outbox, calls requestWithdrawal, tokens burned -->

### Withdrawal Fees

<!-- gasLimit * tempoGasRate, user estimates total gas -->

### Withdrawal Batching

<!-- finalizeWithdrawalBatch at end of final block, hash chain construction, withdrawalBatchIndex ordering -->

### Withdrawal Queue

<!-- Fixed-size ring buffer (capacity 100), head/tail, slot mechanics, diagram -->

### Withdrawal Processing

<!-- processWithdrawal on Tempo, hash verification, unconditional pop -->

### Callback Withdrawals

<!-- ZoneMessenger relay, atomic transfer + callback, IWithdrawalReceiver -->

### Withdrawal Failures and Bounce-Back

<!-- Failure reasons, bounce-back via re-deposit to fallbackRecipient -->

### Authenticated Withdrawals

<!-- senderTag commitment (keccak256(sender, txHash)), revealTo public key, encryptedSender field. 
Two disclosure modes: manual reveal (share txHash off-chain) and encrypted reveal (holder of revealTo key decrypts).
Trust model: sequencer computes senderTag and encryptedSender, trusted to do so correctly (modest extension of existing trust).
Impact on callback withdrawals: onWithdrawalReceived receives bytes32 senderTag instead of address sender.
-->

### Zone-to-Zone Transfers

<!--
- Headline feature: withdraw from Zone A, deposit into Zone B in one flow
- Sender on Zone A sets revealTo = Zone B sequencer's public key
- Withdrawal processed on Tempo with callback data that deposits into Zone B's portal
- Zone B's sequencer decrypts encryptedSender to learn (sender, txHash), verifies against senderTag
- Enables sender-aware processing on Zone B
- Sequencer encryption keys are already published (used for encrypted deposits), no extra infra needed
- Generalizes beyond zone-to-zone: withdraw + swap on Tempo DEX + deposit into another zone
-->

<br>

## Zone Precompiles

<!-- Zone-specific precompiles beyond the standard Tempo TIP-20 precompile. These are deployed at fixed addresses in the 0x1c... range. -->

### TIP-20 Token Precompile

<!--
- Same address as on Tempo, modified for privacy zones
- balanceOf/allowance access control (self or sequencer only)
- Fixed 100k gas for transfer-family operations
- System mint (ZoneInbox only) and burn (ZoneOutbox only)
- Link to execution.md for full details
-->

### Chaum-Pedersen Verify

<!--
- Address: 0x1c00000000000000000000000000000000000100
- Interface: verifyProof(ephemeralPub, sharedSecret, sequencerPub, proof) → bool
- Purpose: prove ECDH shared secret was correctly derived without exposing sequencer private key
- Protocol: R1 = s*G - c*pubSeq, R2 = s*ephemeralPub - c*sharedSecretPoint, recompute challenge
- Gas cost: ~8000
-->

### AES-GCM Decrypt

<!--
- Address: 0x1c00000000000000000000000000000000000101
- Interface: decrypt(key, nonce, ciphertext, aad, tag) → (plaintext, valid)
- Purpose: symmetric decryption for encrypted deposit verification
- Gas cost: ~1000 base + ~500 per 32 bytes
- HKDF-SHA256 key derivation is done in Solidity using SHA256 precompile (0x02)
-->

<br>

## Contracts and Interfaces

### Common Types

<!-- Deposit, Withdrawal, EncryptedDeposit, EncryptedDepositPayload, DecryptionData, ChaumPedersenProof, BlockTransition, DepositQueueTransition, TokenConfig, ZoneInfo, ZoneParams, LastBatch -->

### IZoneFactory

<!-- Solidity interface -->

### IZonePortal

<!-- Solidity interface -->

### IZoneMessenger

<!-- Solidity interface -->

### IWithdrawalReceiver

<!-- Solidity interface -->

### ITempoState

<!-- Solidity interface, address, how reads work -->

### IZoneInbox

<!-- Solidity interface, address -->

### IZoneOutbox

<!-- Solidity interface, address -->

### IZoneConfig

<!-- Solidity interface, address, reads sequencer from L1 -->

### TIP-403 Registry

<!-- Read-only on zone, reads policy from Tempo via TempoState -->

<br>

## Network Upgrades and Hard Fork Activation

<!-- Brief summary of activation rule, verifier routing, two-verifier invariant. Link to upgrades.md for full process -->

<br>

# Security Considerations

<!-- Consolidated: sequencer trust, verifier trust anchor, encrypted deposit trust, bounce-back guarantees, TIP-403 policy changes, token pause effects -->

<br>

# Open Questions

<!-- Cancellable deposits? Portal interface changes across forks? Predeploy upgrade mechanism? -->
