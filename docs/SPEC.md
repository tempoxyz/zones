# Tempo Zones Protocol Specification (Draft)

This document is the canonical technical specification for Tempo Zones — native validium chains anchored to Tempo. For explanatory documentation, see [docs.tempo.xyz](https://docs.tempo.xyz/protocol/zones).

## Table of Contents

1. [Terminology](#1-terminology)
2. [Zone Creation](#2-zone-creation)
3. [Execution Environment](#3-execution-environment)
4. [Fees](#4-fees)
5. [Deposit Queue](#5-deposit-queue)
6. [Withdrawal Queue](#6-withdrawal-queue)
7. [Queue Design Rationale](#7-queue-design-rationale)
8. [Batch Submission and Proving](#8-batch-submission-and-proving)
9. [RPC Specification](#9-rpc-specification)
10. [Zone Predeploys](#10-zone-predeploys)
11. [Tempo Contracts](#11-tempo-contracts)
12. [Queue Libraries](#12-queue-libraries)
13. [Hard Fork Activation](#13-hard-fork-activation)
14. [Data Availability and Liveness](#14-data-availability-and-liveness)
15. [Security Considerations](#15-security-considerations)
16. [Open Questions](#16-open-questions)

---

## 1. Terminology

- **Tempo**: the base chain (L1).
- **Zone**: the validium chain anchored to Tempo.
- **Enabled tokens**: TIP-20 tokens the sequencer has enabled for bridging. Enablement is permanent (append-only).
- **Portal**: the Tempo-side contract that escrows enabled tokens and finalizes exits.
- **Batch**: a sequencer-produced commitment covering one or more zone blocks. Must end with a single `finalizeWithdrawalBatch()` call in the final block; intermediate blocks must not call it.

### Actors

- **Zone sequencer**: permissioned operator that orders zone transactions, provides data, and posts batches to Tempo. Only actor that submits transactions to the portal.
- **Verifier**: ZK proof system or TEE attester. Abstracted via `IVerifier`.
- **Users**: deposit TIP-20 from Tempo to the zone or exit back to Tempo.

---

## 2. Zone Creation

A zone is created via `ZoneFactory.createZone(...)` with:

- `initialToken`: the first Tempo TIP-20 address to enable.
- `sequencer`: permissioned sequencer address.
- `verifier`: `IVerifier` implementation.
- `zoneParams`: initial configuration (genesis block hash, genesis Tempo block hash/number).

The factory deploys a `ZonePortal` and `ZoneMessenger`. The initial token is automatically enabled.

### Chain ID

Each zone has a unique EIP-155 chain ID:

```
chain_id = 4217000000 + zone_id
```

The prefix `4217` corresponds to the Tempo L1 chain ID.

### Token Management

```solidity
struct TokenConfig {
    bool enabled;          // true once sequencer enables this token (permanent)
    bool depositsActive;   // sequencer can pause/unpause deposits
}
```

| Function | Behavior |
|----------|----------|
| `enableToken(address token)` | Enable a TIP-20 for bridging. **Irreversible**. |
| `pauseDeposits(address token)` | Pause new deposits. Withdrawals continue. |
| `resumeDeposits(address token)` | Resume deposits for a previously paused token. |

**Non-custodial withdrawal guarantee**: once enabled, a token can never be disabled. The sequencer can halt deposits but can never prevent withdrawals.

### Sequencer Transfer

Two-step process on Tempo L1:

1. Current sequencer calls `ZonePortal.transferSequencer(newSequencer)`.
2. New sequencer calls `ZonePortal.acceptSequencer()`.

Zone-side system contracts read the sequencer from L1 via `ZoneConfig`, which queries `TempoState` for the sequencer address from the finalized `ZonePortal` storage.

---

## 3. Execution Environment

Privacy zones enforce privacy at two complementary layers: EVM execution and RPC access control. Neither is sufficient alone.

- **Execution alone is insufficient.** Without RPC restrictions, a caller could use `eth_getStorageAt` to read TIP-20 balance mapping slots directly.
- **RPC alone is insufficient.** Without execution-level changes, a caller could use `eth_call` to invoke a contract that reads another account's balance.

### Balance Privacy: `balanceOf` Access Control

- If `msg.sender == account`: call succeeds, returns balance.
- If `msg.sender` is the sequencer (from `ZoneConfig.sequencer()`): call succeeds.
- Otherwise: reverts with `Unauthorized()`.

### Allowance Privacy: `allowance` Access Control

- If `msg.sender == owner` or `msg.sender == spender`: call succeeds.
- If `msg.sender` is the sequencer: call succeeds.
- Otherwise: reverts with `Unauthorized()`.

Public views (`totalSupply()`, `name()`, `symbol()`, `decimals()`) remain unrestricted.

### Fixed Gas Costs

All user-facing TIP-20 operations charge exactly **100,000 gas**:

| Function | Gas Cost |
|----------|----------|
| `transfer(to, amount)` | 100,000 |
| `transferFrom(from, to, amount)` | 100,000 |
| `transferWithMemo(to, amount, memo)` | 100,000 |
| `transferFromWithMemo(from, to, amount, memo)` | 100,000 |
| `approve(spender, amount)` | 100,000 |

System functions (`systemTransferFrom`, `transferFeePreTx`, `transferFeePostTx`) retain standard gas costs.

### System Mint and Burn Permissions

| Operation | Standard TIP-20 (Tempo) | Zone Access |
|-----------|------------------------|-------------|
| `mint(to, amount)` | `ISSUER_ROLE` only | ZoneInbox (`0x1c...0001`) only |
| `burn(from, amount)` | `ISSUER_ROLE` only | ZoneOutbox (`0x1c...0002`) only |

Authorization is operation-specific: ZoneInbox for `mint` only, ZoneOutbox for `burn` only.

**ZoneInbox mints** during deposit processing:

- Regular deposit: `mint(deposit.to, deposit.amount)`
- Encrypted deposit (decryption succeeded): `mint(decrypted.to, deposit.amount)`
- Encrypted deposit (decryption failed): `mint(deposit.sender, deposit.amount)`

**ZoneOutbox burns** during withdrawal requests:

- User approves ZoneOutbox for `amount + fee`.
- ZoneOutbox calls `transferFrom(user, self, amount + fee)`, then `burn(self, amount + fee)`.

`mint` and `burn` retain standard variable gas costs.

### Contract Creation Disabled

`CREATE` and `CREATE2` opcodes are disabled. Any attempt reverts.

---

## 4. Fees

The zone reuses Tempo's fee units and accounting model. Zone transactions specify which enabled TIP-20 to use for gas via a `feeToken` field. The sequencer accepts all enabled tokens directly (no feeAMMs).

### Deposit Fees

```
fee = FIXED_DEPOSIT_GAS × zoneGasRate
```

`FIXED_DEPOSIT_GAS` is fixed at 100,000 gas. The sequencer configures `zoneGasRate` via `ZonePortal.setZoneGasRate()`. The fee is deducted from the deposit amount and paid to the sequencer on Tempo. The deposit queue stores the net amount (`amount - fee`).

### Withdrawal Fees

```
fee = gasLimit × tempoGasRate
```

The user specifies `gasLimit` covering processing + callback. The sequencer configures `tempoGasRate` via `ZoneOutbox.setTempoGasRate()`. Users burn `amount + fee`; on success `amount` goes to recipient and `fee` to sequencer. On failure (bounce-back), only `amount` is re-deposited to `fallbackRecipient`; the sequencer keeps the fee.

---

## 5. Deposit Queue

### Hash Chain Structure (Newest-Outermost)

```
Newest deposit wraps the outside (O(1) addition):

                    ┌─────────────────────────────────────────┐
                    │ hash(d3, ┌─────────────────────────┐ ) │  ← currentDepositQueueHash
                    │          │ hash(d2, ┌───────────┐ ) │  │
                    │          │          │ hash(d1,0) │   │  │
                    │          │          └───────────┘   │  │
                    │          └─────────────────────────┘  │
                    └─────────────────────────────────────────┘
                      ▲                              ▲
                      │                              │
                    newest                        oldest
                   (outermost)                  (innermost)

Adding d4: currentDepositQueueHash = keccak256(abi.encode(deposit, currentDepositQueueHash))
```

On-chain addition is O(1). The zone processes deposits in FIFO order (oldest first) via `advanceTempo()`, validating the result matches `currentDepositQueueHash` from Tempo state.

### Unified Queue

Regular and encrypted deposits share a single ordered queue with a type discriminator:

```solidity
enum DepositType { Regular, Encrypted }

// Regular deposit hash:
keccak256(abi.encode(DepositType.Regular, deposit, prevHash))

// Encrypted deposit hash:
keccak256(abi.encode(DepositType.Encrypted, encryptedDeposit, prevHash))
```

### Common Types

```solidity
interface IZoneToken {
    function mint(address to, uint256 amount) external;
    function burn(address from, uint256 amount) external;
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

struct Deposit {
    address token;
    address sender;
    address to;
    uint128 amount;
    bytes32 memo;
}

/// @notice Queued deposit in the unified deposit queue
struct QueuedDeposit {
    DepositType depositType;
    bytes depositData; // abi.encode(Deposit) or abi.encode(EncryptedDeposit)
}
```

### Encrypted Deposits

**Encryption scheme**: ECIES with secp256k1.

1. Sequencer publishes a secp256k1 encryption public key via `setSequencerEncryptionKey(x, yParity, popV, popR, popS)` with a proof of possession.
2. User generates an ephemeral keypair and derives a shared secret via ECDH.
3. User encrypts `(to || memo)` with AES-256-GCM using the derived key.
4. User calls `depositEncrypted(token, amount, keyIndex, encryptedPayload)` on the portal.

```solidity
struct EncryptedDepositPayload {
    bytes32 ephemeralPubkeyX;
    uint8 ephemeralPubkeyYParity;
    bytes ciphertext;
    bytes12 nonce;
    bytes16 tag;
}

struct EncryptedDeposit {
    address token;
    address sender;
    uint128 amount;
    uint256 keyIndex;
    EncryptedDepositPayload encrypted;
}
```

| Field | Visibility | Reason |
|-------|------------|--------|
| `token` | Public | Needed for on-chain escrow accounting |
| `sender` | Public | Needed for refunds if decryption fails |
| `amount` | Public | Needed for on-chain accounting |
| `to` | Encrypted | Only sequencer knows recipient |
| `memo` | Encrypted | Only sequencer knows payment context |

If decryption fails, the zone mints tokens to `sender`'s address on the zone. L1 funds remain escrowed.

### On-Chain Decryption Verification

The sequencer provides the ECDH shared secret alongside decrypted data:

```solidity
struct ChaumPedersenProof {
    bytes32 s; // Response: s = r + c * privSeq (mod n)
    bytes32 c; // Challenge: c = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)
}

struct DecryptionData {
    bytes32 sharedSecret;
    uint8 sharedSecretYParity;
    address to;
    bytes32 memo;
    ChaumPedersenProof cpProof;
}
```

**Chaum-Pedersen proof protocol:**

1. Prover (sequencer) computes off-chain:
   - Pick random `r`
   - `R1 = r * G`
   - `R2 = r * ephemeralPub`
   - `c = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)` (Fiat-Shamir challenge)
   - `s = r + c * privSeq (mod n)`
   - Proof is `(s, c)`

2. Verifier (on-chain) checks:
   - Reconstruct: `R1 = s*G - c*pubSeq`
   - Reconstruct: `R2 = s*ephemeralPub - c*sharedSecretPoint`
   - Recompute: `c' = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)`
   - Verify: `c == c'`

**Verification in ZoneInbox.advanceTempo():**

```solidity
// Step 1: Look up sequencer's public key from on-chain key history
(bytes32 seqPubX, uint8 seqPubYParity) = _readEncryptionKey(ed.keyIndex);

// Step 2: Verify Chaum-Pedersen proof of correct shared secret derivation
bool proofValid = IChaumPedersenVerify(CHAUM_PEDERSEN_VERIFY).verifyProof(
    ed.encrypted.ephemeralPubX,
    ed.encrypted.ephemeralPubYParity,
    dec.sharedSecret,
    dec.sharedSecretYParity,
    seqPubX,
    seqPubYParity,
    dec.cpProof
);
if (!proofValid) revert InvalidSharedSecretProof();

// Step 3: Derive AES key from shared secret using HKDF-SHA256 (in Solidity)
bytes32 aesKey = _hkdfSha256(
    dec.sharedSecret,
    "ecies-aes-key",
    abi.encodePacked(tempoPortal, ed.keyIndex, ed.encrypted.ephemeralPubkeyX)
);

// Step 4: Decrypt using AES-256-GCM precompile
(bytes memory decryptedPlaintext, bool valid) = IAesGcmDecrypt(AES_GCM_DECRYPT).decrypt(
    aesKey,
    ed.encrypted.nonce,
    ed.encrypted.ciphertext,
    "",
    ed.encrypted.tag
);

// Step 5: Verify decrypted plaintext matches claimed (to, memo)
// Plaintext is packed as [address(20 bytes)][memo(32 bytes)][padding(12 bytes)] = 64 bytes
if (valid && decryptedPlaintext.length == ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE) {
    (address decryptedTo, bytes32 decryptedMemo) = EncryptedDepositLib.decodePlaintext(decryptedPlaintext);
    valid = (decryptedTo == dec.to && decryptedMemo == dec.memo);
} else {
    valid = false;
}

// Step 6: Handle success or failure
if (!valid) {
    IZoneToken(ed.token).mint(ed.sender, ed.amount);
    emit EncryptedDepositFailed(currentHash, ed.sender, ed.token, ed.amount);
} else {
    IZoneToken(ed.token).mint(dec.to, ed.amount);
    emit EncryptedDepositProcessed(currentHash, ed.sender, dec.to, ed.token, ed.amount, dec.memo);
}
```

### Precompiles

**Chaum-Pedersen Verify** (`0x1c00000000000000000000000000000000000100`):

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

Gas cost: ~8000 gas (2 point multiplications + 2 point additions + hash).

**AES-GCM Decrypt** (`0x1c00000000000000000000000000000000000101`):

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

Gas cost: ~1000 gas base + ~500 per 32 bytes of ciphertext.

**HKDF-SHA256** (implemented in Solidity using SHA256 precompile `0x02`):

- HMAC-SHA256: `HMAC(key, msg) = SHA256((key ⊕ opad) || SHA256((key ⊕ ipad) || msg))`
- HKDF-Extract: `PRK = HMAC-SHA256(salt, IKM)`
- HKDF-Expand: `OKM = HMAC-SHA256(PRK, info || 0x01)`
- Gas cost: ~5-10k gas.

### Encryption Key History

```solidity
struct EncryptionKeyEntry {
    bytes32 x;
    uint8 yParity;
    uint64 activationBlock;
}
```

Key management functions:

- `setSequencerEncryptionKey(x, yParity, popV, popR, popS)` — Appends new key, active from current Tempo block. Proof of possession: ECDSA signature over `keccak256(abi.encode(portalAddress, x, yParity))`.
- `encryptionKeyCount()` — Total keys in history.
- `encryptionKeyAt(index)` — Historical key by index.
- `encryptionKeyAtBlock(tempoBlockNumber)` — Key active at a specific block.
- `isEncryptionKeyValid(keyIndex)` — Whether key can be used for new deposits.

**Key expiration**: `ENCRYPTION_KEY_GRACE_PERIOD = 86400` blocks (~1 day). When a new key is set, the previous key remains valid for this period. The current (latest) key never expires. Deposits using an expired key are rejected with `EncryptionKeyExpired(keyIndex, activationBlock, supersededAtBlock)`.

---

## 6. Withdrawal Queue

### Ring Buffer Design

Fixed-size ring buffer with `WITHDRAWAL_QUEUE_CAPACITY = 100`. Each batch gets its own slot. Head and tail are raw counters; modular arithmetic (`index % 100`) is used for slot indexing.

```
Fixed-size ring buffer (WITHDRAWAL_QUEUE_CAPACITY = 100):

     head                              tail
      │                                 │
      ▼                                 ▼
  ┌─────┬─────┬─────┬─────┬─────┬─────┐
  │ w1  │ w4  │ w6  │EMPTY│EMPTY│     │  ... (100 slots, indexed via % 100)
  │ w2  │ w5  │     │     │     │     │
  │ w3  │     │     │     │     │     │
  └─────┴─────┴─────┴─────┴─────┴─────┘
  slot 0 slot 1 slot 2 ...        slot 99

- Batches write to slots[tail % 100], then tail++
- Sequencer processes from slots[head % 100], then head++ when slot exhausted
- Reverts with WithdrawalQueueFull if tail - head >= 100
```

`EMPTY_SENTINEL = 0xffffffff...fff` (not `0x00`, to avoid storage clearing and gas refund issues).

### Hash Chain Structure (Oldest-Outermost)

```
Oldest withdrawal on the outside (O(1) removal):

                    ┌────────────────────────────────────────────────────┐
                    │ hash(w1, ┌──────────────────────────────────────┐) │  ← slots[head]
                    │          │ hash(w2, ┌───────────────────────┐ ) │  │
                    │          │          │ hash(w3, EMPTY_SENTINEL) │  │  │
                    │          │          └───────────────────────┘  │  │
                    │          └──────────────────────────────────────┘  │
                    └────────────────────────────────────────────────────┘
                      ▲                                     ▲
                      │                                     │
                    oldest                              newest
                   (outermost)                       (innermost)

Removing w1: verify hash(w1, remainingQueue) == slots[head], then slots[head] = remainingQueue
When slot exhausted: slots[head] = EMPTY_SENTINEL, head++
```

### Withdrawal Struct

```solidity
struct Withdrawal {
    address token;
    bytes32 senderTag;          // keccak256(abi.encodePacked(sender, txHash))
    address to;
    uint128 amount;             // amount to send to recipient (excludes fee)
    uint128 fee;                // processing fee for sequencer
    bytes32 memo;
    uint64 gasLimit;            // max gas for IWithdrawalReceiver callback (0 = no callback)
    address fallbackRecipient;  // zone address for bounce-back if call fails
    bytes callbackData;         // calldata for IWithdrawalReceiver (max 1KB)
    bytes encryptedSender;      // ECDH-encrypted (sender, txHash) for revealTo key, or empty
}
```

### Withdrawal Processing

```solidity
function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external onlySequencer {
    uint256 head = _withdrawalQueue.head;

    if (head == _withdrawalQueue.tail) {
        revert NoWithdrawalsInQueue();
    }

    uint256 slotIndex = head % WITHDRAWAL_QUEUE_CAPACITY;
    bytes32 currentSlot = _withdrawalQueue.slots[slotIndex];

    bytes32 expectedRemainingQueue = remainingQueue == bytes32(0) ? EMPTY_SENTINEL : remainingQueue;
    require(keccak256(abi.encode(w, expectedRemainingQueue)) == currentSlot, "invalid");

    // Pop unconditionally
    if (remainingQueue == bytes32(0)) {
        _withdrawalQueue.slots[slotIndex] = EMPTY_SENTINEL;
        _withdrawalQueue.head = head + 1;
    } else {
        _withdrawalQueue.slots[slotIndex] = remainingQueue;
    }

    if (w.gasLimit == 0) {
        ITIP20(w.token).transfer(w.to, w.amount);
        return;
    }

    try IZoneMessenger(messenger).relayMessage(w.token, w.senderTag, w.to, w.amount, w.gasLimit, w.callbackData) {
        // Success
    } catch {
        _enqueueBounceBack(w.token, w.amount, w.fallbackRecipient);
    }
}
```

### Bounce-Back

```solidity
function _enqueueBounceBack(address token, uint128 amount, address fallbackRecipient) internal {
    Deposit memory d = Deposit({
        token: token,
        sender: address(this),
        to: fallbackRecipient,
        amount: amount,
        memo: bytes32(0)
    });
    currentDepositQueueHash = keccak256(abi.encode(DepositType.Regular, d, currentDepositQueueHash));
    emit BounceBack(...);
}
```

### Withdrawal Failure Reasons

- **Transfer failure**: `transfer` or `transferFrom` reverts.
- **TIP-403 policy**: Recipient not authorized under the token's transfer policy.
- **Token paused**: The token is globally paused.
- **Callback revert**: Receiver contract reverts (out of gas, logic error, etc.).
- **Callback rejection**: Receiver returns wrong selector.

Zone creators SHOULD choose tokens with `transferPolicyId == 1` to avoid complexity. If using restricted policies, the portal address MUST be whitelisted.

### Authenticated Withdrawals

The sequencer computes both fields when building the withdrawal in `finalizeWithdrawalBatch`:

```
senderTag       = keccak256(abi.encodePacked(sender, txHash))
encryptedSender = ECDH_Encrypt((sender, txHash), revealTo)   // empty if no revealTo
```

`txHash` acts as a 32-byte blinding factor — private to the zone, known only to the sender and sequencer.

#### Reveal Key

The sender specifies an optional `revealTo` public key when requesting the withdrawal:

```solidity
function requestWithdrawal(
    address token,
    address to,
    uint128 amount,
    bytes32 memo,
    uint64 gasLimit,
    address fallbackRecipient,
    bytes calldata data,
    bytes calldata revealTo     // compressed secp256k1 public key (33 bytes), or empty
) external;
```

#### Encrypted Sender Format

When `revealTo` is specified:

```
ephemeralPubKey (33 bytes) || ciphertext (52 bytes) || mac (16 bytes)
```

The sequencer generates ephemeral keypair `(r, R = r*G)`, derives shared secret `S = r * revealTo`, encrypts `abi.encodePacked(sender, txHash)` (52 bytes).

#### Selective Disclosure

- **Manual reveal** (`revealTo` empty): Sender reveals `txHash` off-chain. Verifier checks `keccak256(abi.encodePacked(sender_address, txHash)) == senderTag`.
- **Encrypted reveal** (`revealTo` specified): Holder of `revealTo` private key decrypts `encryptedSender` to obtain `(sender, txHash)`.

#### Zone-to-Zone Transfers

For cross-zone withdrawals (Zone A → Zone B), sender sets `revealTo = pubKeySeqB`. Zone B's sequencer decrypts `encryptedSender`, verifies against `senderTag`, and can attribute the deposit to the sender.

#### Trust Model

The sequencer computes `senderTag` and `encryptedSender` and is trusted to do so correctly. The struct is hashed into the withdrawal queue chain committed in the batch proof. To upgrade to trustless: move `senderTag` computation into the ZK circuit.

#### Impact on Callbacks

`IWithdrawalReceiver.onWithdrawalReceived` receives `bytes32 senderTag` instead of `address sender`. Receivers needing identity can decrypt `encryptedSender` off-chain or receive `txHash` via `callbackData`.

#### Zone-Side Changes

`ZoneOutbox.requestWithdrawal` records the pending withdrawal with plaintext `sender` and `revealTo`. The sequencer computes `senderTag` and `encryptedSender` in `finalizeWithdrawalBatch`. Zone-side `WithdrawalRequested` event continues to include plaintext `sender` (zone events are private).

---

## 7. Queue Design Rationale

|                      | Deposit Queue | Withdrawal Queue |
|----------------------|---------------|------------------|
| On-chain operation   | Add (users deposit) | Remove (sequencer processes) |
| Proven operation     | Remove (zone consumes) | Add (zone creates) |
| Efficient on-chain   | Addition | Removal |
| Stable proving target| For removals | For additions |

Both use hash chains structured so the **on-chain operation touches the outermost layer**: additions wrap the outside, removals unwrap from the outside. The expensive operation (processing the full queue) happens inside the ZKP where O(N) is acceptable.

---

## 8. Batch Submission and Proving

### submitBatch Fields

| Field | Description |
|-------|-------------|
| `tempoBlockNumber` | Tempo block the zone committed to (from zone's TempoState) |
| `recentTempoBlockNumber` | Optional recent block for ancestry proof (`0` = direct lookup) |
| `blockTransition` | Zone block hash transition (`prevBlockHash` → `nextBlockHash`) |
| `depositQueueTransition` | Deposit queue processing progress |
| `withdrawalQueueHash` | Hash chain of withdrawals for this batch (`0` if none) |
| `verifierConfig` | Opaque payload for the verifier |
| `proof` | Validity proof or TEE attestation |

### IVerifier Interface

```solidity
struct BlockTransition {
    bytes32 prevBlockHash;
    bytes32 nextBlockHash;
}

struct DepositQueueTransition {
    bytes32 prevProcessedHash;
    bytes32 nextProcessedHash;
}

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

The verifier validates:

1. Valid state transition from `prevBlockHash` to `nextBlockHash`.
2. Zone committed to `tempoBlockNumber` via TempoState.
3. **Direct mode** (`anchorBlockNumber == tempoBlockNumber`): zone's `tempoBlockHash()` matches `anchorBlockHash`.
4. **Ancestry mode** (`anchorBlockNumber > tempoBlockNumber`): parent hash chain verified inside proof.
5. `ZoneOutbox.lastBatch()` has correct `withdrawalBatchIndex` and `withdrawalQueueHash`.
6. Deposit processing is correct (zone read `currentDepositQueueHash` from Tempo state).
7. Zone block `beneficiary` matches sequencer.

### Ancestry Proofs

EIP-2935 provides access to the last ~8,192 block hashes. If `tempoBlockNumber` rotates out, ancestry proofs prevent zone bricking:

1. Portal reads `recentTempoBlockNumber` hash from EIP-2935.
2. Prover includes Tempo headers from `tempoBlockNumber + 1` to `recentTempoBlockNumber`.
3. Proof verifies parent hash chain inside the ZK circuit.
4. Portal verifies constant-size proof against the recent block hash.

| Mode | Condition | Behavior |
|------|-----------|----------|
| Direct | `recentTempoBlockNumber = 0` | Portal reads `tempoBlockNumber` hash from EIP-2935 |
| Ancestry | `recentTempoBlockNumber > tempoBlockNumber` | Portal reads `recentTempoBlockNumber` hash; proof verifies parent chain |

Constraints:
- `recentTempoBlockNumber` must be strictly greater than `tempoBlockNumber` (equal reverts; use `0` for direct mode).
- Both must be `>= genesisTempoBlockNumber`.

Proving time increases linearly with the block gap. On-chain verification cost remains constant.

### State Transition Function

```rust
#![no_std]

pub fn prove_zone_batch(witness: BatchWitness) -> Result<BatchOutput, Error>
```

#### Rust Types

```rust
pub struct PublicInputs {
    pub prev_block_hash: B256,
    pub tempo_block_number: u64,
    pub anchor_block_number: u64,
    pub anchor_block_hash: B256,
    pub expected_withdrawal_batch_index: u64,
    pub sequencer: Address,
}

pub struct BatchWitness {
    pub public_inputs: PublicInputs,
    pub prev_block_header: ZoneHeader,
    pub zone_blocks: Vec<ZoneBlock>,
    pub initial_zone_state: ZoneStateWitness,
    pub tempo_state_proofs: BatchStateProof,
    pub tempo_ancestry_headers: Vec<Vec<u8>>,
}

pub struct BatchOutput {
    pub block_transition: BlockTransition,
    pub deposit_queue_transition: DepositQueueTransition,
    pub withdrawal_queue_hash: B256,
    pub last_batch: LastBatchCommitment,
}

pub struct LastBatchCommitment {
    pub withdrawal_batch_index: u64,
}

pub struct LastBatch {
    pub withdrawal_queue_hash: B256,
    pub withdrawal_batch_index: u64,
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
    pub number: u64,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub beneficiary: Address,
    pub protocol_version: u64,
    pub tempo_header_rlp: Option<Vec<u8>>,
    pub deposits: Vec<QueuedDeposit>,
    pub decryptions: Vec<DecryptionData>,
    pub finalize_withdrawal_batch_count: Option<U256>,
    pub transactions: Vec<Transaction>,
}

pub struct QueuedDeposit {
    pub deposit_type: DepositType,
    pub deposit_data: Vec<u8>,
}

pub enum DepositType {
    Regular,  // corresponds to Solidity DepositType.Regular
    Encrypted,
}

pub struct DecryptionData {
    pub shared_secret: B256,
    pub to: Address,
    pub memo: B256,
    pub cp_proof: ChaumPedersenProof,
}

pub struct ChaumPedersenProof {
    pub s: B256,
    pub c: B256,
}

pub struct ZoneStateWitness {
    pub accounts: HashMap<Address, AccountWitness>,
    pub state_root: B256,
}

pub struct AccountWitness {
    pub nonce: u64,
    pub balance: U256,
    pub code_hash: B256,
    pub code: Option<Vec<u8>>,
    pub storage: HashMap<U256, U256>,
    pub account_proof: Vec<Vec<u8>>,
    pub storage_proofs: HashMap<U256, Vec<Vec<u8>>>,
}

pub struct BatchStateProof {
    pub node_pool: HashMap<B256, Vec<u8>>,
    pub reads: Vec<L1StateRead>,
}

pub struct L1StateRead {
    pub zone_block_index: u64,
    pub tempo_block_number: u64,
    pub account: Address,
    pub slot: U256,
    pub node_path: Vec<B256>,
    pub value: U256,
}

#[derive(Debug)]
pub enum Error {
    InvalidProof,
    ExecutionError(String),
    InconsistentState,
}
```

The witness only includes accounts and storage slots accessed during batch execution. Any access not present must be treated as an error (do not default to zero).

#### Tempo State Binding

- Tempo headers are validated whenever `ZoneInbox.advanceTempo` executes, updating `tempoBlockNumber`, `tempoBlockHash`, and `tempoStateRoot`.
- `TempoState.tempoBlockNumber()` at end of batch must equal `public_inputs.tempo_block_number`.
- Each Tempo read is verified against the `tempoStateRoot` bound at the time of the read.
- If a block contains no `advanceTempo`, reads use the binding from the previous block.
- Reads inside `advanceTempo` must be bound to the header finalized by that call.

#### Deduplication Strategy

All proofs share a single deduplicated node pool instead of separate MPT proofs per read.

```
Traditional approach:
  - 100,000 reads × 8 nodes = 800,000 keccak operations, ~25.6 MB

Deduplicated approach:
  - ~50,000 unique nodes verified once each
  - ~16x smaller, ~16x faster
```

#### Execution Flow

```rust
pub fn prove_zone_batch(witness: BatchWitness) -> Result<BatchOutput, Error> {
    // Phase 1: Verify Tempo state proofs
    let tempo_state = verify_tempo_proofs(&witness.tempo_state_proofs)?;

    // Phase 2: Initialize zone state
    let mut zone_state = ZoneState::from_witness(&witness.initial_zone_state)?;

    if zone_state.state_root() != witness.prev_block_header.state_root {
        return Err(Error::InconsistentState);
    }
    if keccak256(rlp_encode(&witness.prev_block_header)) != witness.public_inputs.prev_block_hash {
        return Err(Error::InvalidProof);
    }

    let deposit_prev = zone_state.zone_inbox_processed_hash()?;

    // Phase 3: Execute zone blocks
    let mut prev_block_hash = witness.public_inputs.prev_block_hash;
    let mut prev_header = witness.prev_block_header;

    for (idx, block) in witness.zone_blocks.iter().enumerate() {
        let is_last_block = idx + 1 == witness.zone_blocks.len();

        if block.parent_hash != prev_block_hash {
            return Err(Error::InconsistentState);
        }
        if block.number != prev_header.number + 1 {
            return Err(Error::InconsistentState);
        }
        if block.timestamp < prev_header.timestamp {
            return Err(Error::InconsistentState);
        }
        if block.beneficiary != witness.public_inputs.sequencer {
            return Err(Error::InconsistentState);
        }

        if is_last_block {
            if block.finalize_withdrawal_batch_count.is_none() {
                return Err(Error::InconsistentState);
            }
        } else if block.finalize_withdrawal_batch_count.is_some() {
            return Err(Error::InconsistentState);
        }

        let (tx_root, receipts_root) =
            execute_zone_block(&mut zone_state, block, &tempo_state, idx)?;

        let header = ZoneHeader {
            parent_hash: prev_block_hash,
            beneficiary: block.beneficiary,
            state_root: zone_state.state_root(),
            transactions_root: tx_root,
            receipts_root: receipts_root,
            number: block.number,
            timestamp: block.timestamp,
            protocol_version: block.protocol_version,
        };
        prev_block_hash = keccak256(rlp_encode(header));
        prev_header = header;
    }

    // Phase 4: Extract output commitments
    let deposit_next = zone_state.zone_inbox_processed_hash()?;
    let last_batch = zone_state.zone_outbox_last_batch()?;
    let tempo_number = zone_state.tempo_state_block_number()?;

    let tempo_hash = tempo_state
        .block_hash(tempo_number)
        .ok_or(Error::InvalidProof)?;
    if tempo_number != witness.public_inputs.tempo_block_number {
        return Err(Error::InconsistentState);
    }

    if witness.public_inputs.anchor_block_number == tempo_number {
        if tempo_hash != witness.public_inputs.anchor_block_hash {
            return Err(Error::InconsistentState);
        }
    } else {
        verify_tempo_ancestry_chain(
            tempo_hash,
            tempo_number,
            witness.public_inputs.anchor_block_number,
            witness.public_inputs.anchor_block_hash,
            &witness.tempo_ancestry_headers,
        )?;
    }

    Ok(BatchOutput {
        block_transition: BlockTransition {
            prev_block_hash: witness.public_inputs.prev_block_hash,
            next_block_hash: prev_block_hash,
        },
        deposit_queue_transition: DepositQueueTransition {
            prev_processed_hash: deposit_prev,
            next_processed_hash: deposit_next,
        },
        withdrawal_queue_hash: last_batch.withdrawal_queue_hash,
        last_batch: LastBatchCommitment {
            withdrawal_batch_index: last_batch.withdrawal_batch_index,
        },
    })
}

fn verify_tempo_proofs(
    proofs: &BatchStateProof,
) -> Result<TempoStateAccessor, Error> {
    let mut verified_nodes = HashMap::new();
    for (claimed_hash, rlp_data) in &proofs.node_pool {
        let actual_hash = keccak256(rlp_data);
        if actual_hash != *claimed_hash {
            return Err(Error::InvalidProof);
        }
        verified_nodes.insert(*claimed_hash, MptNode::decode(rlp_data)?);
    }

    let mut read_index = HashMap::new();
    for read in &proofs.reads {
        read_index.insert(
            (read.zone_block_index, read.account, read.slot),
            read,
        );
    }

    Ok(TempoStateAccessor { verified_nodes, read_index })
}

fn execute_zone_block(
    zone_state: &mut ZoneState,
    block: &ZoneBlock,
    tempo_state: &TempoStateAccessor,
    block_index: usize,
) -> Result<(B256, B256), Error> {
    let mut evm = revm::EVM::builder()
        .with_db(zone_state)
        .with_block_env(block_env_from(block))
        .with_precompile(
            TEMPO_STATE_ADDRESS,
            TempoStatePrecompile::new(tempo_state, block_index),
        )
        .build();

    if let Some(tempo_header_rlp) = &block.tempo_header_rlp {
        evm.transact_commit(system_tx_advance_tempo(
            tempo_header_rlp,
            &block.deposits,
            &block.decryptions,
        ))
        .map_err(|e| Error::ExecutionError(e.to_string()))?;

        let tempo_number = zone_state.tempo_state_block_number()?;
        tempo_state.bind_block(block_index, tempo_number)?;

        let expected_tempo_hash = tempo_state
            .block_hash(tempo_number)
            .ok_or(Error::InvalidProof)?;
        if expected_tempo_hash != keccak256(tempo_header_rlp) {
            return Err(Error::InconsistentState);
        }
    } else if !block.deposits.is_empty() || !block.decryptions.is_empty() {
        return Err(Error::InconsistentState);
    }

    for tx in &block.transactions {
        evm.transact_commit(tx)
            .map_err(|e| Error::ExecutionError(e.to_string()))?;
    }

    if let Some(count) = block.finalize_withdrawal_batch_count {
        evm.transact_commit(sequencer_tx_finalize_withdrawal_batch(count))
            .map_err(|e| Error::ExecutionError(e.to_string()))?;
    }

    let tx_root = compute_transactions_root_from_block(block);
    let receipts_root = compute_receipts_root(evm.last_block_receipts());

    Ok((tx_root, receipts_root))
}
```

### Deployment Modes

**ZKVM (SP1):**

```rust
#[cfg(target_os = "zkvm")]
fn main() {
    let witness: BatchWitness = zkvm::io::read();
    let output = prove_zone_batch(witness).expect("proof generation failed");
    zkvm::io::commit(&output);
}
```

**TEE (SGX/TDX):**

```rust
#[cfg(target_env = "sgx")]
#[no_mangle]
pub extern "C" fn ecall_prove_batch(
    witness_ptr: *const u8,
    witness_len: usize,
) -> BatchOutput {
    let witness = unsafe { deserialize(witness_ptr, witness_len) };
    prove_zone_batch(witness).expect("proof generation failed")
}
```

### Block Header Commitments

| Field | In Hash | Proven On-Chain | Purpose |
|-------|---------|-----------------|---------|
| `parentHash` | ✓ | ✓ | Chain continuity |
| `beneficiary` | ✓ | ✓ | Sequencer validation |
| `stateRoot` | ✓ | ✓ | Core state commitment |
| `transactionsRoot` | ✓ | ✗ | Prevents sequencer revisionism |
| `receiptsRoot` | ✓ | ✗ | Committed but batch params read from state |
| `number` | ✓ | ✓ | Block ordering |
| `timestamp` | ✓ | ✓ | Monotonicity |
| `protocolVersion` | ✓ | ✓ | Fork validation |
| `gasLimit` | ✗ | N/A | Omitted — zones have no hard gas limit |
| `gasUsed` | ✗ | N/A | Omitted |
| `logsBloom` | ✗ | N/A | Omitted |
| `extraData` | ✗ | N/A | Omitted |

---

## 9. RPC Specification

### Authorization Tokens

Every RPC request must include an authorization token in the `X-Authorization-Token` HTTP header. The token proves the caller controls a Tempo account and scopes all responses to that account.

#### Message Format

```solidity
bytes32 authorizationTokenHash = keccak256(abi.encodePacked(
    bytes32(0x54656d706f5a6f6e65525043),  // "TempoZoneRPC" magic prefix
    uint8(version),                         // spec version (currently 0)
    uint32(zoneId),                         // zone this key is valid for (0 = unscoped)
    uint64(chainId),                        // zone chain ID
    uint64(issuedAt),                       // unix timestamp (seconds)
    uint64(expiresAt)                       // unix timestamp (seconds)
));
```

`version` MUST be `0`. Unrecognized versions MUST be rejected.

#### Unscoped Tokens

`zoneId == 0` indicates a token valid for any zone (zone IDs start at 1). The RPC server MUST skip the zone ID check. All other validation still applies.

#### Signature Types

| Type | Detection | Address Derivation |
|------|-----------|-------------------|
| **secp256k1** | Exactly 65 bytes, no type prefix | `ecrecover` |
| **P256** | First byte `0x01`, 130 bytes total | `address(uint160(uint256(keccak256(abi.encodePacked(pubKeyX, pubKeyY)))))` |
| **WebAuthn** | First byte `0x02`, variable length (max 2KB) | Same as P256 |
| **Keychain** | First byte `0x03` (V1) or `0x04` (V2), variable length | Authenticated account is `user_address` field |

**WebAuthn verification:**

Verified:
- `authenticatorData` minimum 37 bytes.
- UP or UV flag set.
- AT flag NOT set.
- ED flag NOT set.
- `clientDataJSON.type` equals `"webauthn.get"`.
- `clientDataJSON.challenge` matches `authorizationTokenHash` (Base64URL, no padding).
- P256 signature valid over `sha256(authenticatorData || sha256(clientDataJSON))`.

Intentionally skipped: RP ID hash, `clientDataJSON.origin`, signature counter.

**Keychain Access Keys:**

Accounts with Access Keys via the zone's `AccountKeychain` precompile can authenticate. The zone has its own independent `AccountKeychain` (not mirrored from Tempo L1).

```
keychain_signature_v1 = 0x03 || user_address (20 bytes) || inner_signature
keychain_signature_v2 = 0x04 || user_address (20 bytes) || inner_signature
```

V2 binds `user_address` into the signing hash: `keccak256(0x04 || authorizationTokenHash || user_address)`.

Server steps:
1. Parse signature using Tempo transaction rules.
2. Verify inner signature.
3. Derive signing key address.
4. Query `AccountKeychain.getKey(user_address, keyId)` — must be active, non-expired, non-revoked.
5. Verify stored `signatureType` matches inner signature type.
6. Set authenticated account to `user_address`.

#### Validation

Reject tokens where:
- `zoneId` ≠ zone's configured `zoneId` AND `zoneId` ≠ `0`.
- `chainId` ≠ zone's configured chain ID.
- `expiresAt - issuedAt > 2592000` (max 30 days).
- `expiresAt <= now`.
- `issuedAt > now + 60` (60s clock skew tolerance).
- Signature malformed or invalid.
- Keychain key not authorized, revoked, or expired.

#### Transport

```
POST /
X-Authorization-Token: <hex-encoded>
Content-Type: application/json
```

Wire format:
```
<signature bytes><version: 1 byte><zoneId: 4 bytes><chainId: 8 bytes><issuedAt: 8 bytes><expiresAt: 8 bytes>
```

Token fields are always exactly 29 bytes. Parse by reading the **last 29 bytes**; everything before is the signature.

- No token: `401 Unauthorized`.
- Expired/malformed/unauthorized: `403 Forbidden`.
- Keychain verification failure (cannot read precompile): `500 Internal Server Error`.

#### Sequencer Access

When the authenticated account equals the sequencer address, all restrictions are lifted.

### Method Access Control

**Default deny**: Any method not listed MUST return `-32601` (method not found).

#### Allowed Methods

| Method | Notes |
|--------|-------|
| `eth_chainId` | Zone chain ID |
| `eth_blockNumber` | Latest block number |
| `eth_gasPrice` | Current gas price |
| `eth_maxPriorityFeePerGas` | Current priority fee |
| `eth_feeHistory` | Fee history |
| `eth_getBlockByNumber` | Headers without transaction details |
| `eth_getBlockByHash` | Headers without transaction details |
| `eth_subscribe("newHeads")` | Headers with `logsBloom` zeroed |
| `eth_syncing` | Sync status |
| `eth_coinbase` | Sequencer address |
| `net_version` | Network ID |
| `net_listening` | Node status |
| `web3_clientVersion` | Client version |
| `web3_sha3` | Keccak-256 hash |

#### Scoped Methods

**State queries:**

| Method | Scoping Rule |
|--------|-------------|
| `eth_getBalance` | Authenticated account only. Other accounts return `0x0`. |
| `eth_getTransactionCount` | Authenticated account only. Other accounts return `0x0`. |
| `eth_call` | `from` set to authenticated account. State overrides rejected for non-sequencer (`-32602`). |
| `eth_estimateGas` | `from` must equal authenticated account. State overrides rejected for non-sequencer (`-32602`). |

**Transaction access:**

| Method | Scoping Rule |
|--------|-------------|
| `eth_getTransactionByHash` | Returns tx only if authenticated account is sender. Otherwise `null`. |
| `eth_getTransactionReceipt` | Returns receipt only if authenticated account is sender. Logs filtered. |
| `eth_sendRawTransaction` | Sender must match authenticated account. Mismatch: `-32003`. |

**Transaction simulation:**

`eth_call` and `eth_estimateGas`: `from` MUST equal authenticated account (omitted defaults to authenticated account; mismatch returns `-32004`).

**Event filtering:**

| Method | Scoping Rule |
|--------|-------------|
| `eth_getLogs` | TIP-20 events where authenticated account is a relevant party. |
| `eth_getFilterLogs` | Same. |
| `eth_getFilterChanges` | Same. |
| `eth_newFilter` | Implicitly scoped to authenticated account. |
| `eth_subscribe("logs")` | Same. |
| `eth_newBlockFilter` | Allowed. |
| `eth_uninstallFilter` | Allowed. |

**Error vs. silent response**: Mismatched explicit parameters (`eth_sendRawTransaction`, `eth_call`) return errors. Queries about other accounts return dummy values (`0x0`, `null`, `[]`).

#### Timing Side Channels

Mandatory **100 ms minimum response time** on:

| Method | Reason |
|--------|--------|
| `eth_getTransactionByHash` | Must fetch tx to check sender |
| `eth_getTransactionReceipt` | Must fetch receipt to check sender |
| `eth_getLogs` | Response time correlates with total volume |
| `eth_getFilterLogs` | Same |
| `eth_getFilterChanges` | Same |

Not needed for: `eth_getBalance`, `eth_getTransactionCount` (address checked before fetch), `eth_call`/`eth_estimateGas` (`from` validated before execution), `eth_sendRawTransaction` (sender verified during decoding).

#### Restricted Methods (Sequencer-Only)

| Method | Reason |
|--------|--------|
| `eth_getStorageAt` | Raw storage reads bypass access control |
| `eth_getCode` | No legitimate non-sequencer use case |
| `eth_createAccessList` | Reveals storage layout |
| `eth_getBlockByNumber` (with `true`) | Full block with all transactions |
| `eth_getBlockByHash` (with `true`) | Full block with all transactions |
| `eth_getBlockTransactionCountByNumber` | Transaction counts reveal activity |
| `eth_getBlockTransactionCountByHash` | Same |
| `eth_getTransactionByBlockNumberAndIndex` | Arbitrary transaction access |
| `eth_getTransactionByBlockHashAndIndex` | Same |
| `eth_getBlockReceipts` | All receipts, bypasses per-sender scoping |
| `eth_getUncleCountByBlockNumber` | Restricted for consistency |
| `eth_getUncleCountByBlockHash` | Same |
| `debug_*` | All debug methods |
| `admin_*` | All admin methods |
| `txpool_*` | Transaction pool inspection |

#### Disabled Methods

| Method | Reason |
|--------|--------|
| `eth_getUncleByBlockNumberAndIndex` | No uncles |
| `eth_getUncleByBlockHashAndIndex` | No uncles |
| `eth_mining` / `eth_hashrate` / `eth_getWork` / `eth_submitWork` / `eth_submitHashrate` | No mining |
| `eth_getProof` | Merkle proofs leak state trie structure |
| `eth_newPendingTransactionFilter` | Mempool observation |
| `eth_subscribe("newPendingTransactions")` | Mempool observation |

Disabled methods return `-32601`.

### Block Responses

**Non-sequencer callers:**
- `transactions`: always empty array `[]`.
- `logsBloom`: replaced with zero Bloom (512 zero bytes).
- All other header fields returned normally.

**Sequencer callers:** full block data including all transactions and receipts.

### Event Filtering Rules

| Event | Visible If |
|-------|-----------|
| `Transfer(address indexed from, address indexed to, uint256 amount)` | `from == caller` OR `to == caller` |
| `Approval(address indexed owner, address indexed spender, uint256 amount)` | `owner == caller` OR `spender == caller` |
| `TransferWithMemo(address indexed from, address indexed to, uint256 amount, bytes32 indexed memo)` | `from == caller` OR `to == caller` |
| `Mint(address indexed to, uint256 amount)` | `to == caller` |
| `Burn(address indexed from, uint256 amount)` | `from == caller` |

All other event topics filtered out.

**Filter enforcement:**
1. `address` parameter MUST be zone token address or omitted.
2. RPC server appends topic filter restricting indexed addresses to authenticated account.
3. Post-filtering removes any log where authenticated account is not a relevant party.

### Zone-Specific RPC Methods

#### `zone_getAuthorizationTokenInfo`

```json
// Request
{"method": "zone_getAuthorizationTokenInfo", "params": []}

// Response
{"account": "0x1234...", "expiresAt": "0x67d2d7c0"}
```

#### `zone_getZoneInfo`

```json
// Request
{"method": "zone_getZoneInfo", "params": []}

// Response
{
  "zoneId": "0x1",
  "zoneTokens": ["0x20c0000000000000000000000000000000000000"],
  "sequencer": "0xabcd...",
  "chainId": "0x2a"
}
```

#### `zone_getDepositStatus(tempoBlockNumber)`

```json
// Request
{"method": "zone_getDepositStatus", "params": ["0x2a"]}

// Response
{
  "tempoBlockNumber": "0x2a",
  "zoneProcessedThrough": "0x2a",
  "processed": true,
  "deposits": [{
    "depositHash": "0xfeed...",
    "kind": "regular",
    "token": "0x20c0000000000000000000000000000000000000",
    "sender": "0xaaaa...",
    "recipient": "0xbbbb...",
    "amount": "0xf4240",
    "memo": "0x1111...",
    "status": "processed"
  }]
}
```

Visibility rules:
- Regular deposits: returned only when caller is sender or recipient.
- Encrypted deposits: returned immediately to sender. Returned to recipient only after `EncryptedDepositProcessed` emitted on L2.
- Pending encrypted deposits MUST keep `recipient` and `memo` as `null`.

**Withdrawals**: MUST use `eth_sendRawTransaction` with a signed transaction calling `ZoneOutbox.requestWithdrawal(...)`. Authorization tokens are read-only credentials and MUST NOT authorize state changes.

### Error Codes

| Code | Message | Meaning |
|------|---------|---------|
| `-32001` | Authorization token required | No token provided |
| `-32002` | Authorization token expired | Token expired |
| `-32003` | Transaction rejected | Sender mismatch (`eth_sendRawTransaction`) |
| `-32004` | Account mismatch | `from` mismatch (`eth_call`, `eth_estimateGas`) |
| `-32005` | Sequencer only | Requires sequencer access |
| `-32006` | Method disabled | Not available on zones |

### Security Considerations

- **Timing**: 100 ms response floor on scoped methods that fetch before checking authorization.
- **Nonce privacy**: `eth_getTransactionCount` returns `0x0` for non-authenticated accounts.
- **Token replay**: Scoped to zone and network, max 30 days, read-only. Server SHOULD implement rate limiting.
- **Simulation overrides**: MUST reject state/block override sets for non-sequencer callers.
- **Keychain revocation**: MUST stop accepting revoked keys within 1 second. Cache TTL bounded by `min(token.expiresAt, keyExpiry)`. Recommended: event-driven eviction via `KeyRevoked` events.
- **P256/WebAuthn**: Public key transmitted in clear (not a security concern).
- **Metadata leakage**: Deployments SHOULD use TLS.
- **Fixed gas**: Ensures identical `gasUsed` in receipts for all transfers.
- **Block sanitization**: `logsBloom` zeroed for non-sequencer callers.

### Implementation Notes

- Filter state stored per-authenticated-account. Filters accessible across authorization tokens for same account.
- SHOULD cache authorization token verification. Keychain cache entries MUST honor key expiry and be invalidated within 1 second of revocation.
- P256/WebAuthn verification is expensive — aggressively cache by token hash.
- WebSocket: authorization token provided during handshake, connection terminated on expiry or key revocation.

---

## 10. Zone Predeploys

| Contract | Address | Purpose |
|----------|---------|---------|
| `TempoState` | `0x1c00000000000000000000000000000000000000` | Stores zone's view of Tempo. Sequencer updates with Tempo headers. |
| `ZoneInbox` | `0x1c00000000000000000000000000000000000001` | Processes incoming deposits. |
| `ZoneOutbox` | `0x1c00000000000000000000000000000000000002` | Handles withdrawal requests. |
| `ZoneConfig` | `0x1c00000000000000000000000000000000000003` | Central configuration. Reads sequencer and token registry from Tempo. |

### Zone Token Model

No TIP-20 factory on zones. All tokens are bridged representations at the **same address** as on Tempo. Zone tokens are precompiles provisioned by the zone node. Total supply controlled by bridge: `ZoneInbox` mints on deposit, `ZoneOutbox` burns on withdrawal.

### IZoneConfig

```solidity
interface IZoneConfig {
    error NotSequencer();
    error NoEncryptionKeySet();

    function tempoPortal() external view returns (address);
    function tempoState() external view returns (ITempoState);
    function sequencer() external view returns (address);
    function pendingSequencer() external view returns (address);
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);
    function isSequencer(address account) external view returns (bool);
    function isEnabledToken(address token) external view returns (bool);
}
```

Reads sequencer and token registry from finalized L1 `ZonePortal` storage via `TempoState.readTempoStorageSlot()`.

### ITempoState

```solidity
interface ITempoState {
    event TempoBlockFinalized(bytes32 indexed blockHash, uint64 indexed blockNumber, bytes32 stateRoot);

    function tempoBlockHash() external view returns (bytes32);

    // Tempo wrapper fields
    function generalGasLimit() external view returns (uint64);
    function sharedGasLimit() external view returns (uint64);

    // Inner Ethereum header fields
    function tempoParentHash() external view returns (bytes32);
    function tempoBeneficiary() external view returns (address);
    function tempoStateRoot() external view returns (bytes32);
    function tempoTransactionsRoot() external view returns (bytes32);
    function tempoReceiptsRoot() external view returns (bytes32);
    function tempoBlockNumber() external view returns (uint64);
    function tempoGasLimit() external view returns (uint64);
    function tempoGasUsed() external view returns (uint64);
    function tempoTimestamp() external view returns (uint64);
    function tempoTimestampMillis() external view returns (uint64);
    function tempoPrevRandao() external view returns (bytes32);

    function finalizeTempo(bytes calldata header) external;

    /// @dev RESTRICTED: Only callable by zone system contracts (ZoneInbox, ZoneOutbox, ZoneConfig).
    function readTempoStorageSlot(address account, bytes32 slot) external view returns (bytes32);
    function readTempoStorageSlots(address account, bytes32[] calldata slots) external view returns (bytes32[] memory);
}
```

Tempo headers are RLP encoded as `rlp([general_gas_limit, shared_gas_limit, timestamp_millis_part, inner])`, where `inner` is a standard Ethereum header. Optional trailing fields (EIP-1559, EIP-4895, EIP-4844, EIP-4788, EIP-7685) are skipped.

`readTempoStorageSlot` functions are precompile stubs restricted to system contracts only — actual implementation is in the zone node, validated against `tempoStateRoot`.

### TIP-403 Registry

The zone has a `TIP403Registry` at the same address as Tempo. Read-only — its `isAuthorized` reads policy state from Tempo via the Tempo state reader precompile.

### IZoneInbox

```solidity
interface IZoneInbox {
    event TempoAdvanced(
        bytes32 indexed tempoBlockHash,
        uint64 indexed tempoBlockNumber,
        uint256 depositsProcessed,
        bytes32 newProcessedDepositQueueHash
    );

    event DepositProcessed(
        bytes32 indexed depositHash,
        address indexed sender,
        address indexed to,
        address token,
        uint128 amount,
        bytes32 memo
    );

    event EncryptedDepositProcessed(
        bytes32 indexed depositHash,
        address indexed sender,
        address indexed to,
        address token,
        uint128 amount,
        bytes32 memo
    );

    event EncryptedDepositFailed(
        bytes32 indexed depositHash, address indexed sender, address token, uint128 amount
    );

    error OnlySequencer();
    error InvalidDepositQueueHash();
    error MissingDecryptionData();
    error ExtraDecryptionData();
    error InvalidSharedSecretProof();

    function config() external view returns (IZoneConfig);
    function tempoPortal() external view returns (address);
    function tempoState() external view returns (ITempoState);
    function processedDepositQueueHash() external view returns (bytes32);

    function advanceTempo(
        bytes calldata header,
        QueuedDeposit[] calldata deposits,
        DecryptionData[] calldata decryptions
    ) external;
}
```

### IZoneOutbox

```solidity
struct LastBatch {
    bytes32 withdrawalQueueHash;
    uint64 withdrawalBatchIndex;
}

interface IZoneOutbox {
    function MAX_CALLBACK_DATA_SIZE() external view returns (uint256);

    event WithdrawalRequested(
        uint64 indexed withdrawalIndex,
        address indexed sender,
        address token,
        address to,
        uint128 amount,
        uint128 fee,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes data,
        bytes revealTo
    );

    event TempoGasRateUpdated(uint128 tempoGasRate);

    event BatchFinalized(
        bytes32 indexed withdrawalQueueHash,
        uint64 withdrawalBatchIndex
    );

    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    function tempoGasRate() external view returns (uint128);
    function nextWithdrawalIndex() external view returns (uint64);
    function withdrawalBatchIndex() external view returns (uint64);
    function lastBatch() external view returns (LastBatch memory);
    function pendingWithdrawalsCount() external view returns (uint256);

    function transferSequencer(address newSequencer) external;
    function acceptSequencer() external;
    function setTempoGasRate(uint128 _tempoGasRate) external;

    function maxWithdrawalsPerBlock() external view returns (uint256);
    function setMaxWithdrawalsPerBlock(uint256 _maxWithdrawalsPerBlock) external;

    function calculateWithdrawalFee(uint64 gasLimit) external view returns (uint128);

    function requestWithdrawal(
        address token,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes calldata data,
        bytes calldata revealTo
    ) external;

    function finalizeWithdrawalBatch(uint256 count) external returns (bytes32 withdrawalQueueHash);
}
```

`finalizeWithdrawalBatch()` constructs the hash chain on-chain by processing withdrawals in reverse order (newest to oldest), so the oldest ends up outermost for O(1) Tempo removal:

```
withdrawalQueueHash = EMPTY_SENTINEL
for i from (pendingCount - 1) down to 0:
    withdrawalQueueHash = keccak256(abi.encode(withdrawals[i], withdrawalQueueHash))
    pop withdrawal from storage
```

---

## 11. Tempo Contracts

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
        uint32 indexed zoneId,
        address indexed portal,
        address indexed messenger,
        address initialToken,
        address sequencer,
        address verifier,
        bytes32 genesisBlockHash,
        bytes32 genesisTempoBlockHash,
        uint64 genesisTempoBlockNumber
    );

    function createZone(CreateZoneParams calldata params) external returns (uint32 zoneId, address portal);
    function zoneCount() external view returns (uint32);
    function zones(uint32 zoneId) external view returns (ZoneInfo memory);
    function isZonePortal(address portal) external view returns (bool);
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
```

### IZonePortal

```solidity
interface IZonePortal {
    event DepositMade(
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed sender,
        address token,
        address to,
        uint128 netAmount,
        uint128 fee,
        bytes32 memo
    );

    event BatchSubmitted(
        uint64 indexed withdrawalBatchIndex,
        bytes32 nextProcessedDepositQueueHash,
        bytes32 nextBlockHash,
        bytes32 withdrawalQueueHash
    );

    event WithdrawalProcessed(
        address indexed to,
        uint128 amount,
        bool callbackSuccess
    );

    event BounceBack(
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed fallbackRecipient,
        uint128 amount
    );

    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    event EncryptedDepositMade(
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed sender,
        address token,
        uint128 netAmount,
        uint128 fee,
        uint256 keyIndex,
        bytes32 ephemeralPubkeyX,
        uint8 ephemeralPubkeyYParity,
        bytes ciphertext,
        bytes12 nonce,
        bytes16 tag
    );

    event SequencerEncryptionKeyUpdated(
        bytes32 x, uint8 yParity, uint256 keyIndex, uint64 activationBlock
    );
    event ZoneGasRateUpdated(uint128 zoneGasRate);

    error NotSequencer();
    error NotPendingSequencer();
    error InvalidProof();
    error InvalidTempoBlockNumber();
    error CallbackRejected();
    error EncryptionKeyExpired(uint256 keyIndex, uint64 activationBlock, uint64 supersededAtBlock);
    error InvalidEncryptionKeyIndex(uint256 keyIndex);
    error NoEncryptionKeySet();
    error NoEncryptionKeyAtBlock(uint64 blockNumber);
    error InvalidEphemeralPubkey();
    error InvalidCiphertextLength(uint256 actual, uint256 expected);
    error InvalidProofOfPossession();
    error DepositTooSmall();

    function FIXED_DEPOSIT_GAS() external view returns (uint64);
    function zoneId() external view returns (uint64);
    function messenger() external view returns (address);
    function sequencer() external view returns (address);
    function pendingSequencer() external view returns (address);
    function zoneGasRate() external view returns (uint128);
    function verifier() external view returns (address);
    function genesisTempoBlockNumber() external view returns (uint64);

    function isTokenEnabled(address token) external view returns (bool);
    function areDepositsActive(address token) external view returns (bool);
    function enabledTokenCount() external view returns (uint256);
    function enabledTokenAt(uint256 index) external view returns (address);
    function enableToken(address token) external;
    function pauseDeposits(address token) external;
    function resumeDeposits(address token) external;
    function withdrawalBatchIndex() external view returns (uint64);
    function blockHash() external view returns (bytes32);
    function currentDepositQueueHash() external view returns (bytes32);
    function lastSyncedTempoBlockNumber() external view returns (uint64);
    function withdrawalQueueHead() external view returns (uint256);
    function withdrawalQueueTail() external view returns (uint256);
    function withdrawalQueueSlot(uint256 slot) external view returns (bytes32);

    function transferSequencer(address newSequencer) external;
    function acceptSequencer() external;
    function setZoneGasRate(uint128 _zoneGasRate) external;
    function calculateDepositFee() external view returns (uint128 fee);
    function deposit(address token, address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositQueueHash);
    function depositEncrypted(
        address token,
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata encrypted
    ) external returns (bytes32 newCurrentDepositQueueHash);

    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);
    function setSequencerEncryptionKey(
        bytes32 x, uint8 yParity, uint8 popV, bytes32 popR, bytes32 popS
    ) external;
    function encryptionKeyCount() external view returns (uint256);
    function encryptionKeyAt(uint256 index) external view returns (EncryptionKeyEntry memory);
    function encryptionKeyAtBlock(uint64 tempoBlockNumber)
        external view returns (bytes32 x, uint8 yParity, uint256 keyIndex);
    function isEncryptionKeyValid(uint256 keyIndex)
        external view returns (bool valid, uint64 expiresAtBlock);

    function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external;
    function submitBatch(
        uint64 tempoBlockNumber,
        uint64 recentTempoBlockNumber,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        bytes32 withdrawalQueueHash,
        bytes calldata verifierConfig,
        bytes calldata proof
    ) external;
}
```

### IZoneMessenger

```solidity
interface IZoneMessenger {
    function portal() external view returns (address);

    function relayMessage(
        address token,
        bytes32 senderTag,
        address target,
        uint128 amount,
        uint64 gasLimit,
        bytes calldata data
    ) external;
}
```

The messenger does `ITIP20(token).transferFrom(portal, target, amount)` then calls the target with `data`. Both atomic. Receivers check `msg.sender == zoneMessenger`.

### IWithdrawalReceiver

```solidity
interface IWithdrawalReceiver {
    function onWithdrawalReceived(
        bytes32 senderTag,
        address token,
        uint128 amount,
        bytes calldata callbackData
    ) external returns (bytes4);
}
```

Must return `IWithdrawalReceiver.onWithdrawalReceived.selector`. Wrong selector or revert triggers bounce-back.

### ITIP20 (Minimal)

```solidity
interface ITIP20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function approve(address spender, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}
```

---

## 12. Queue Libraries

### DepositQueueLib

```solidity
library DepositQueueLib {
    /// @dev Hash chain: newHash = keccak256(abi.encode(deposit, prevHash))
    function enqueue(bytes32 currentHash, Deposit memory depositData) internal pure returns (bytes32 newHash);
}
```

### WithdrawalQueueLib

```solidity
bytes32 constant EMPTY_SENTINEL = bytes32(type(uint256).max);
uint256 constant WITHDRAWAL_QUEUE_CAPACITY = 100;

struct WithdrawalQueueTransition {
    bytes32 withdrawalQueueHash; // hash chain for this batch (EMPTY_SENTINEL if none)
}

struct WithdrawalQueue {
    uint256 head;
    uint256 tail;
    mapping(uint256 => bytes32) slots;
}

library WithdrawalQueueLib {
    function enqueue(WithdrawalQueue storage q, WithdrawalQueueTransition memory transition) internal;
    function dequeue(WithdrawalQueue storage q, Withdrawal calldata w, bytes32 remainingQueue) internal;
    function hasWithdrawals(WithdrawalQueue storage q) internal view returns (bool);
    function length(WithdrawalQueue storage q) internal view returns (uint256);
}
```

| Queue | Tempo Operation | Zone/Proof Operation |
|-------|--------------|---------------------|
| Deposit | `enqueue` (users deposit) | Process via `advanceTempo()` |
| Withdrawal | `dequeue` (sequencer processes) | Create via `finalizeWithdrawalBatch()` |

---

## 13. Hard Fork Activation

### Definitions

- **Fork L1 block number (`F`)**: the L1 block at which the fork activates. Embedded in zone node chain spec and prover program; not stored in portal.
- **`forkVerifier`**: new `IVerifier` contract deployed on Tempo L1 as part of the hard fork.
- **Post-fork zone block**: a zone block whose `advanceTempo` imports L1 block `>= F`.
- **Fork-spanning batch**: a batch containing both pre-fork and post-fork zone blocks.

### Activation Rule

The fork activates in the zone block that imports the fork L1 block — same-block activation. The entire zone block uses new execution rules. EVM configured with new ruleset before any transaction executes.

Trigger is L1 block number, not timestamp. If fork changes L1 header format, new parsing code must be active in the block that first encounters it.

### Verifier Routing

The portal maintains internal verifier routing state. The `submitBatch` external interface (see [IZonePortal](#izoneportal)) does not include a `targetVerifier` parameter — verifier selection is handled internally by the portal based on `tempoBlockNumber` and `forkActivationBlock`:

```solidity
// Internal portal state (not part of the external interface)
address public verifier;
address public forkVerifier;
uint64  public forkActivationBlock;

// Internal routing logic within submitBatch:
function _routeVerifier(address targetVerifier, uint64 tempoBlockNumber) internal {
    require(
        targetVerifier == verifier || targetVerifier == forkVerifier,
        "unknown verifier"
    );
    if (targetVerifier == verifier && forkActivationBlock != 0) {
        require(tempoBlockNumber < forkActivationBlock, "use fork verifier");
    }
    require(IVerifier(targetVerifier).verify(...), "invalid proof");
    ...
}
```

### Two-Verifier Invariant

At most two verifiers active at any time. Rotation at each fork:

| Event | `verifier` | `forkVerifier` | `forkActivationBlock` |
|-------|-----------|----------------|----------------------|
| Zone creation | V0 | `address(0)` | 0 |
| Fork 1 (block F1) | V0 | V1 | F1 |
| Fork 2 (block F2) | V1 | V2 | F2 |
| Fork 3 (block F3) | V2 | V3 | F3 |

At each fork:
```
verifier            = forkVerifier
forkVerifier        = new_fork_verifier
forkActivationBlock = block.number
```

Submissions targeting `verifier` must have `tempoBlockNumber < forkActivationBlock`.

### Prover Selection

- **Pre-fork batches** (all zone blocks import L1 `< F`): old prover, old verifier.
- **Post-fork or fork-spanning batches** (any zone block imports L1 `>= F`): new prover, fork verifier. Only submittable after block `F`.

Each prover is a superset of the previous (fork N prover includes fork N-1 logic as old-rules branch).

### Zone Node Behavior

The node inspects the next L1 block before building each zone block. Fork L1 block number embedded in chain spec. If next L1 block `>= F`, EVM configured with new rules.

If fork changes predeploy behavior, new bytecode injected at predeploy addresses before `advanceTempo` executes.

**Fork signaling**: `ZoneFactory.protocolVersion` counter incremented at each fork. Zone node binary embeds `MAX_SUPPORTED_PROTOCOL_VERSION`. If bumped beyond supported version, node halts with upgrade error.

### L1 Hard Fork Actions

1. Deploy fork `IVerifier` contract.
2. Call `ZoneFactory.setForkVerifier(forkVerifier)` — rotates verifiers on all registered portals, updates `_validVerifiers`.
3. Increment `ZoneFactory.protocolVersion`.

### Upgrade Process

#### Artifacts

| Artifact | Embeds `F` | Description |
|----------|:----------:|-------------|
| Zone node binary | Yes | Dual-rule binary. Embeds `F` in chain spec. |
| Prover program | Yes | Dual-rule. Old rules for `< F`, new for `>= F`. |
| Verifier contract | No | `IVerifier` with new verification key. |
| Predeploy bytecode | N/A | Updated predeploy bytecode if needed. |
| L1 system tx payload | No | `setForkVerifier()` + `protocolVersion` increment. |

#### Timeline

```
Pre-fork                          Fork block F                    Post-fork
─────────────────────────────────┬──────────────────────────────┬──────────────────────────
Build artifacts                  │ Deploy verifier contract     │ Zones activate new rules
Release node binary + prover     │ setForkVerifier() rotates    │ Provers use new rules for
Operators upgrade nodes          │   verifiers on all portals   │   post-fork blocks
                                 │ Increment protocolVersion    │ Settlement resumes with
                                 │                              │   fork verifier
```

#### Failure Modes

- **Operator did not upgrade**: Node detects `protocolVersion` exceeds `MAX_SUPPORTED_PROTOCOL_VERSION`, halts. No invalid blocks produced. Settlement pauses. Funds safe.
- **Node upgraded but prover stale**: Node produces correct post-fork blocks but cannot prove them. Settlement of post-fork batches pauses. Once new prover installed, it proves the backlog.
- **Zone behind L1**: Fork does not activate until zone imports block `F`. Zone that falls more than one full fork cycle behind risks having oldest batches become unsubmittable (N-2 verifier deprecated at fork N).

---

## 14. Data Availability and Liveness

- Zone data availability is fully trusted to the sequencer.
- If the sequencer withholds data or halts, users cannot reconstruct zone state or force exits.
- Batch posting and withdrawal processing are sequencer-only.

---

## 15. Security Considerations

- Sequencer can halt the zone without recourse (no data availability).
- The verifier is a trust anchor. A faulty verifier can steal or lock funds.
- Withdrawals with callbacks go through the zone messenger with user-specified gas limit. Transfer + callback are atomic; failure triggers bounce-back.
- Deposits are locked on Tempo until a verified batch consumes them.
- Failed withdrawals bounce back to zone `fallbackRecipient`. Users always retain funds.
- TIP-403 policy changes or token pauses cause affected withdrawals to bounce back.
- Sequencer trust for encrypted deposits: trusted to decrypt correctly and credit the right recipient.

---

## 16. Open Questions

- Should deposits be cancellable if not consumed within a timeout?
- Sequencer-signed tag: per-withdrawal sequencer signature over `senderTag` (~65 bytes overhead).
- `revealTo` for non-zone recipients via ENS or on-chain registry.
- Portal interface changes across forks.
- General predeploy bytecode injection mechanism.
- L1 block number vs timestamp derivation for timestamp-based L1 forks.
