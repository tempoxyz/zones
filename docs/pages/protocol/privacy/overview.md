# Tempo Zones (Draft)

This document proposes a new validium protocol designed for Tempo. It is a design overview, not a full specification.

## Goals

- Create a Tempo-native validium called a zone.
- Each zone has exactly one permissioned sequencer.
- Each zone bridges exactly one TIP-20 token, which is called its *zone token*. The zone token is used to pay transaction fees on the zone.
- Settlement uses fast validity proofs or TEE attestations (ZK or TEE). Data availability is fully trusted to the sequencer.
- Cross-chain operations are Tempo-centric: bridge in (simple deposit), bridge out (with optional callback to receiver contracts for Tempo composability).
- Verifier is abstracted behind a minimal `IVerifier` interface.
- Liveness (including exits) is wholly dependent on the permissioned sequencer; there is no permissionless fallback.

## Non-goals

- No attempt to solve data availability, forced inclusion, or censorship resistance.
- No upgradeability or governance model.
- No general messaging or multi-asset bridging. Only one TIP-20 per zone.

## Terminology

- Tempo: the base chain.
- Zone: the validium chain anchored to Tempo.
- Zone token: the zone's only TIP-20, bridged from Tempo.
- Portal: the Tempo-side contract that escrows the zone token and finalizes exits.
- Batch: a sequencer-produced commitment covering one or more zone blocks. The batch **must** end with a single `finalizeWithdrawalBatch()` call in the final block, and intermediate blocks **must not** call `finalizeWithdrawalBatch()`. The sequencer controls batch frequency.

## System overview

### Actors

- Zone sequencer: permissioned operator that orders zone transactions, provides data, and posts batches to Tempo. The sequencer is the only actor that submits transactions to the portal.
- Verifier: ZK proof system or TEE attester. Abstracted via `IVerifier`.
- Users: deposit TIP-20 from Tempo to the zone or exit back to Tempo.

### Tempo contracts

- `ZoneFactory`: creates zones and registers parameters.
- `ZonePortal`: per-zone portal that escrows the zone token on Tempo and finalizes exits.

### Zone components (off-chain or zone-side)

- `ZoneSequencer`: collects transactions and creates batches.
- `ZoneExecutor`: executes the zone state transition.
- `ZoneProver` or `ZoneAttester`: produces proof/attestation for each batch.

## Zone creation

A zone is created via `ZoneFactory.createZone(...)` with:

- `token`: the Tempo TIP-20 address to bridge. This is the only bridged token and the zone token.
- `sequencer`: permissioned sequencer address.
- `verifier`: `IVerifier` implementation for proof or attestation.
- `zoneParams`: initial configuration (genesis block hash, genesis Tempo block hash/number).

The factory deploys a `ZonePortal` that escrows the zone token on Tempo. The zone genesis includes the portal address and the zone token configuration.

### Sequencer transfer

The sequencer can transfer control to a new address via a two-step process on **Tempo L1 only**:

1. Current sequencer calls `ZonePortal.transferSequencer(newSequencer)` to nominate a new sequencer
2. New sequencer calls `ZonePortal.acceptSequencer()` to accept the transfer

Zone reads sequencer from L1 via ZoneConfig (L1 is the single source of truth).

## Execution and fees

- The zone reuses Tempo's fee units and accounting model.
- The fee token is always the zone token. There is no fee token selection.
- Transactions use Tempo transaction semantics for fee payer, max fee per gas, and gas limit. The fee token field is fixed to the zone token.

### Deposit fees

Deposits incur a processing fee to compensate the sequencer for zone-side processing costs:

- **Zone gas rate**: Sequencer publishes `zoneGasRate` (zone token units per gas unit)
- **Fixed gas value**: `FIXED_DEPOSIT_GAS` is fixed at 100,000 gas
- **Total fee**: `FIXED_DEPOSIT_GAS * zoneGasRate` = `100,000 * zoneGasRate`

The fee is deducted from the deposit amount and paid to the sequencer immediately on Tempo. The deposit queue stores the net amount (`amount - fee`) which is minted on the zone.

### Withdrawal processing fees

Withdrawals incur a processing fee to compensate the sequencer for Tempo-side gas costs:

- **Tempo gas rate**: Sequencer publishes `tempoGasRate` (zone token units per gas unit)
- **Gas limit**: User specifies `gasLimit` covering all execution costs (processing + callback)
- **Total fee**: `gasLimit * tempoGasRate`

Users burn `amount + fee` when requesting a withdrawal. On success, `amount` goes to the recipient and `fee` goes to the sequencer. On failure (bounce-back), only `amount` is re-deposited to `fallbackRecipient`; the sequencer keeps the fee.

## Batch submission

The sequencer posts batches to Tempo via a single `submitBatch` call (sequencer-only) that:

1. Verifies the proof/attestation for the state transition (including chain integrity via `prevBlockHash`).
2. Updates the portal's `withdrawalBatchIndex`, `blockHash`, and `lastSyncedTempoBlockNumber`.
3. Updates the withdrawal queue (adds new withdrawals to the next slot in the unbounded buffer).

Each batch submission includes:

- `tempoBlockNumber` - Block zone committed to (from zone's TempoState)
- `recentTempoBlockNumber` - Optional recent block for ancestry proof (0 = direct lookup)
- `blockTransition` - Zone block hash transition (prevBlockHash → nextBlockHash)
- `depositQueueTransition` - Deposit queue processing (prevProcessedHash → nextProcessedHash)
- `withdrawalQueueTransition` - Withdrawal queue hash (hash chain for this batch, or 0 if none)
- `verifierConfig` - Opaque payload for verifier (domain separation/attestation)
- `proof` - Validity proof or TEE attestation

The portal tracks `withdrawalBatchIndex`, `blockHash` (last proven batch block), `lastSyncedTempoBlockNumber` (Tempo block zone synced to), `currentDepositQueueHash` (deposit queue head), and an unbounded buffer for withdrawals.

If `tempoBlockNumber` is outside the EIP-2935 window, ancestry mode is available (see [Ancestry proofs](#ancestry-proofs-for-historical-blocks) below).

The portal calls the verifier to validate the batch:

```solidity
/// @notice Block transition for zone batch proofs
/// @dev Uses block hash instead of state root to commit to full block structure
struct BlockTransition {
    bytes32 prevBlockHash;
    bytes32 nextBlockHash;
}

/// @notice Deposit queue transition inputs/outputs for batch proofs
/// @dev The proof reads currentDepositQueueHash from Tempo state to validate
///      that nextProcessedHash matches currentDepositQueueHash for now. TODO: allow ancestor checks.
struct DepositQueueTransition {
    bytes32 prevProcessedHash;     // where proof starts (verified against zone state)
    bytes32 nextProcessedHash;     // where zone processed up to (proof output)
}

interface IVerifier {
    /// @notice Verify a batch proof
    /// @dev The proof validates:
    ///      1. Valid state transition from prevBlockHash to nextBlockHash
    ///      2. Zone committed to tempoBlockNumber (via TempoState)
    ///      3. If anchorBlockNumber == tempoBlockNumber: zone's hash matches anchorBlockHash
    ///      4. If anchorBlockNumber > tempoBlockNumber: ancestry chain verified via parent hashes
    ///      5. ZoneOutbox.lastBatch().withdrawalBatchIndex == expectedWithdrawalBatchIndex
    ///      6. ZoneOutbox.lastBatch().withdrawalQueueHash matches withdrawalQueueTransition
    ///      7. Zone block beneficiary matches sequencer
    ///      8. Deposit processing is correct (validated via Tempo state read inside proof)
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
1. State transition from `prevBlockHash` to `nextBlockHash` is correct.
2. Zone committed to `tempoBlockNumber` via TempoState.
3. **Direct mode** (`anchorBlockNumber == tempoBlockNumber`): Zone's `tempoBlockHash()` matches `anchorBlockHash`.
4. **Ancestry mode** (`anchorBlockNumber > tempoBlockNumber`): Proof includes Tempo headers from `tempoBlockNumber + 1` to `anchorBlockNumber` as witness data, verifying the parent hash chain inside the ZK proof. Final hash must equal `anchorBlockHash`.
5. `ZoneOutbox.lastBatch()` has correct `withdrawalBatchIndex` and `withdrawalQueueHash`.
6. Deposit processing is correct (zone read `currentDepositQueueHash` from Tempo state).
7. Zone block `beneficiary` matches sequencer.

The zone has access to Tempo state via the TempoState predeploy, so the proof can read `currentDepositQueueHash` directly from Tempo storage at the proven block. This eliminates the need for an on-chain "ceiling" slot.

`verifierData` + `proof` are opaque to the portal: ZK systems can ignore `verifierData`, while TEEs can pack attestation envelopes/quotes and measurement checks into `verifierData` for the verifier contract to enforce.

`submitBatch` verifies that `prevBlockHash == blockHash`, then calls the verifier. On success it updates `withdrawalBatchIndex`, `blockHash`, `lastSyncedTempoBlockNumber`, adds withdrawals to the queue, and emits `BatchSubmitted` with `withdrawalBatchIndex`, `nextProcessedDepositQueueHash`, `nextBlockHash`, and `withdrawalQueueHash` for off-chain auditing.

### Ancestry proofs for historical blocks

EIP-2935 provides access to the last ~8192 block hashes. If a zone is inactive for longer than this window, `tempoBlockNumber` rotates out of EIP-2935, preventing batch submission and permanently bricking the zone.

**Solution**: The proof verifies ancestry inside the ZK circuit, avoiding expensive on-chain verification:

1. Portal reads `recentTempoBlockNumber` hash from EIP-2935 (must be recent)
2. Prover includes Tempo headers from `tempoBlockNumber + 1` to `recentTempoBlockNumber` as witness data
3. Proof verifies the parent hash chain: each header's parent hash must match the previous header's hash, starting from zone's committed `tempoBlockHash()` and ending at `anchorBlockHash`
4. Portal verifies the (constant-size) proof against the recent block hash

**Usage constraints**:
- `recentTempoBlockNumber = 0` → **direct mode**: portal reads `tempoBlockNumber` hash from EIP-2935
- `recentTempoBlockNumber > tempoBlockNumber` → **ancestry mode**: portal reads `recentTempoBlockNumber` hash, proof verifies parent chain
- `recentTempoBlockNumber` must be **strictly greater** than `tempoBlockNumber` (passing equal values reverts; use `0` for direct mode)
- Both `tempoBlockNumber` and `recentTempoBlockNumber` must be `>= genesisTempoBlockNumber`

**Cost**: Proving time increases linearly with the block gap, but verification remains constant. A 15k block gap adds ~15k keccak operations inside the proof but doesn't increase on-chain gas costs beyond the normal verification.

**Note**: This feature changes the `IVerifier.verify()` signature (adds `anchorBlockNumber` parameter). Verifier implementations must be upgraded alongside the portal.

### Deposit queue

Tempo to zone communication uses a single `depositQueue` chain. Each deposit is hashed into a chain:

```
newHash = keccak256(abi.encode(deposit, prevHash))
```

Where `deposit` is a `Deposit` struct containing the sender, recipient, amount, and memo. Tempo state advancement and deposit processing are combined in the ZoneInbox's `advanceTempo()` function, which calls `TempoState.finalizeTempo()` internally.

The portal tracks `currentDepositQueueHash` where new deposits land. The zone tracks its own `processedDepositQueueHash` in EVM state.

**Proof requirements**: The proof validates deposit processing by reading `currentDepositQueueHash` from Tempo state inside the proof. The zone's `advanceTempo()` function processes deposits and updates the zone's `processedDepositQueueHash`. The proof ensures this was done correctly by validating the Tempo state read. For now, the on-chain inbox requires an exact match; TODO: implement a recursive ancestor check in the proof or on-chain as a fallback.

**After batch accepted**:
1. `lastSyncedTempoBlockNumber = tempoBlockNumber` (record how far Tempo state was synced)

New deposits continue to land in `currentDepositQueueHash` while proofs are in flight. Users can check if their deposit is processed by comparing their deposit's Tempo block number against `lastSyncedTempoBlockNumber`.

Proofs or attestations are assumed to be fast. No data availability is required by the verifier.

## Withdrawal queue

Withdrawals use an unbounded buffer that allows the sequencer to process withdrawals independently of proof generation. Each batch gets its own slot, and the sequencer processes withdrawals from the oldest slot while new batches add to the next available slot.

The portal tracks:
- `head` - slot index of the oldest unprocessed batch (where sequencer removes)
- `tail` - slot index where the next batch will write (where proofs add)
- `maxSize` - maximum queue length ever reached (for gas accounting)
- `slots` - mapping of slot index to hash chain (`EMPTY_SENTINEL` = empty)

**Gas note**: Since this is implemented as a precompile on Tempo, storage gas should only be charged when `(tail - head) > maxSize`, i.e., when the queue length exceeds its previous maximum. This allows the queue to shrink and regrow without repeated storage charges.

### Hash chain structure

Each slot contains a hash chain with the **oldest withdrawal at the outermost layer**, making FIFO processing efficient. The innermost element wraps `EMPTY_SENTINEL` (0xffffffff...fff) instead of 0x00 to avoid clearing storage:

```
slot = keccak256(abi.encode(w1, keccak256(abi.encode(w2, keccak256(abi.encode(w3, EMPTY_SENTINEL))))))
      // w1 is oldest (outermost), w3 is newest (innermost)
```

The sequencer processes withdrawals via `processWithdrawal()`, which verifies the hash, pops the withdrawal unconditionally, and handles callbacks (see [Withdrawal execution](#withdrawal-execution) below).

### Batch submission adds withdrawals

When a batch is submitted with withdrawals, they go into the slot at `tail`, then `tail` advances:

```solidity
function submitBatch(...) external onlySequencer {
    // ... verify proof ...

    // If no withdrawals in this batch, nothing to do
    if (withdrawalQueueTransition.withdrawalQueueHash == bytes32(0)) {
        return;
    }

    uint256 tail = _withdrawalQueue.tail;

    // Write the withdrawal hash chain to this slot
    _withdrawalQueue.slots[tail] = withdrawalQueueTransition.withdrawalQueueHash;

    // Advance tail
    _withdrawalQueue.tail = tail + 1;

    // Update maxSize if current queue length exceeds previous maximum
    uint256 currentSize = _withdrawalQueue.tail - _withdrawalQueue.head;
    if (currentSize > _withdrawalQueue.maxSize) {
        _withdrawalQueue.maxSize = currentSize;
    }
}
```

This design eliminates race conditions entirely - each batch has its own independent slot, and the sequencer processes slots in order. The unbounded buffer means the queue can never be "full".

## Interfaces and types

Canonical Solidity definitions live in [`docs/specs/src/zone/IZone.sol`](../../../specs/src/zone/IZone.sol). Key interfaces:

- `IZoneToken` — Zone token (TIP-20 with mint/burn for system)
- `IVerifier` — Batch proof/attestation verification
- `IZoneFactory` — Zone creation and registry
- `IZonePortal` — Per-zone portal (escrow, deposits, withdrawals, batch submission)
- `IZoneMessenger` — Cross-chain message delivery for withdrawal callbacks
- `IWithdrawalReceiver` — Callback interface for composable withdrawals

Key types: `ZoneInfo`, `ZoneParams`, `Deposit`, `EncryptedDeposit`, `Withdrawal`, `BlockTransition`, `DepositQueueTransition`

Zone-side system contracts (predeploys):
- `TempoState` (`0x1c00...0000`) — Tempo state verification
- `ZoneInbox` (`0x1c00...0001`) — Tempo state advancement and deposit processing
- `ZoneOutbox` (`0x1c00...0002`) — Withdrawal requests and batch finalization
- `ZoneConfig` (`0x1c00...0003`) — Zone configuration (reads sequencer from L1)

## Queue design rationale

Both the deposit queue and withdrawal queue are FIFO queues that require constant on-chain storage. They have symmetric but inverted requirements:

|                      | Deposit queue | Withdrawal queue |
|----------------------|---------------|------------------|
| On-chain operation   | Add (users deposit) | Remove (sequencer processes) |
| Proven operation     | Remove (zone consumes) | Add (zone creates) |
| Efficient on-chain   | Addition | Removal |
| Stable proving target| For removals | For additions |

Both use hash chains, but with different models:

- **Deposit queue**: Tempo tracks only `currentDepositQueueHash` (where new deposits land). The zone tracks its own `processedDepositQueueHash` in EVM state. The proof validates deposit processing by reading `currentDepositQueueHash` from Tempo state inside the proof.
- **Withdrawal queue**: unbounded buffer (each batch gets its own slot, `head` points to oldest unprocessed batch, `tail` points to where next batch writes, `maxSize` tracks peak queue length for gas accounting)

The hash chains are structured differently to optimize for their on-chain operation:

### Deposit queue: newest-outermost

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

Adding d4: currentDepositQueueHash = keccak256(abi.encode(deposit4, currentDepositQueueHash))
```

- **On-chain addition is O(1)**: `currentDepositQueueHash = keccak256(abi.encode(deposit, currentDepositQueueHash))` — wrap the outside.
- **Zone processing**: The zone's `advanceTempo()` processes deposits in FIFO order (oldest first, working outward from its `processedDepositQueueHash`), and validates the result matches `currentDepositQueueHash` (read from Tempo state). TODO: implement a recursive ancestor check in the proof or on-chain as a fallback.
- **After batch**: Tempo updates `lastSyncedTempoBlockNumber` to record how far Tempo state was synced.

### Withdrawal queue: oldest-outermost per slot

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

- **On-chain removal is O(1)**: Sequencer provides withdrawal + remaining hash, portal verifies and unwraps one layer.
- **Proving additions**: Proof builds queue with new withdrawals at innermost (O(N) inside ZKP), writes to slot at tail.
- **Unbounded buffer**: Each batch gets its own slot. Sequencer processes from `head`, proofs add at `tail`. The `maxSize` field tracks peak queue length for gas accounting.

```
Unbounded buffer:

     head                              tail
      │                                 │
      ▼                                 ▼
  ┌─────┬─────┬─────┬─────┬─────┬─────┬─────┬─────┐
  │ w1  │ w4  │ w6  │EMPTY│EMPTY│EMPTY│     │     │  ...unbounded
  │ w2  │ w5  │     │     │     │     │     │     │
  │ w3  │     │     │     │     │     │     │     │
  └─────┴─────┴─────┴─────┴─────┴─────┴─────┴─────┘
  slot 0 slot 1 slot 2 ...

- Batches write to slots[tail], then tail++
- Sequencer processes from slots[head], then head++ when slot exhausted
- maxSize updated when (tail - head) exceeds previous maximum
- Gas only charged for new storage when queue length exceeds maxSize
```

The key insight: structure the hash chain so the **on-chain operation touches the outermost layer**. Additions wrap the outside; removals unwrap from the outside. The expensive operation (processing the full queue) happens inside the ZKP where O(N) is acceptable. Using `EMPTY_SENTINEL` (0xffffffff...fff) instead of 0x00 avoids storage clearing and gas refund incentive issues.

## Bridging in (Tempo to zone)

1. User calls `ZonePortal.deposit(to, amount, memo)` on Tempo.
2. `ZonePortal` transfers `amount` of the zone token into escrow and appends a deposit to the queue: `currentDepositQueueHash = keccak256(abi.encode(deposit, currentDepositQueueHash))`.
3. The sequencer observes `DepositMade` events and processes deposits in order via `ZoneInbox.advanceTempo()`, crediting `to` with `amount` of the zone token (TIP-20 balance). Deposits always succeed—there is no callback or bounce mechanism.
4. A batch proof/attestation must prove the zone correctly processed deposits by validating the Tempo state read inside the proof.
5. After the batch is accepted, `lastSyncedTempoBlockNumber` is updated to record how far Tempo state was synced.

Notes:

- Deposits are simple token credits. There are no callbacks or failure modes on the zone side.
- Deposits are finalized for Tempo once the batch is verified.
- There is no forced inclusion. If the sequencer withholds deposits, funds are stuck in escrow.
- The portal only stores `currentDepositQueueHash`, not individual deposits. The sequencer must track deposits off-chain.
- Tempo state advancement is combined with deposit processing in `ZoneInbox.advanceTempo()`, which calls `TempoState.finalizeTempo()` internally.
- The proof validates an exact match to `currentDepositQueueHash` from Tempo state, ensuring it cannot claim to process fake deposits. TODO: implement a recursive ancestor check in the proof or on-chain as a fallback.

### Encrypted deposits

For privacy-sensitive use cases, users can make **encrypted deposits** where the recipient and memo are encrypted using the sequencer's public key. Only the sequencer can decrypt and credit the correct recipient on the zone.

**Encryption scheme**: ECIES with secp256k1

1. Sequencer publishes a secp256k1 encryption public key via `setSequencerEncryptionKey(x, yParity, popV, popR, popS)` with a proof of possession
2. User generates an ephemeral keypair and derives a shared secret via ECDH
3. User encrypts `(to || memo)` with AES-256-GCM using the derived key
4. User calls `depositEncrypted(amount, keyIndex, encryptedPayload)` on the portal

```solidity
/// @notice Encrypted deposit payload
struct EncryptedDepositPayload {
    bytes32 ephemeralPubkeyX;     // Ephemeral public key X coordinate (for ECDH)
    uint8 ephemeralPubkeyYParity; // Y coordinate parity (0x02 or 0x03)
    bytes ciphertext;             // AES-256-GCM encrypted (to || memo || padding)
    bytes12 nonce;                // GCM nonce
    bytes16 tag;                  // GCM authentication tag
}

/// @notice Encrypted deposit stored in the queue
struct EncryptedDeposit {
    address sender;              // Depositor (public, for refunds)
    uint128 amount;              // Amount (public, for accounting)
    EncryptedDepositPayload encrypted; // Encrypted (to, memo)
}
```

**What's public vs. private:**

| Field | Visibility | Reason |
|-------|------------|--------|
| `sender` | Public | Needed for potential refunds if decryption fails |
| `amount` | Public | Needed for on-chain accounting/escrow |
| `to` | Encrypted | Privacy - only sequencer knows recipient |
| `memo` | Encrypted | Privacy - only sequencer knows payment context |

**Processing flow:**

1. User calls `depositEncrypted(amount, keyIndex, encrypted)` on Tempo portal
2. Portal escrows funds, adds to the **unified deposit queue**, and emits `EncryptedDepositMade`
3. Sequencer decrypts the payload off-chain using their private key
4. When processing the zone block, sequencer calls `advanceTempo()` with deposits from the unified queue
5. For each encrypted deposit, sequencer provides decrypted `(to, memo)` alongside the encrypted data
6. Zone/proof validates decryption and credits the recipient

**Unified deposit queue:**

Regular and encrypted deposits share a single ordered queue with a type discriminator in the hash chain:

```solidity
enum DepositType { Regular, Encrypted }

// Regular deposit hash:
keccak256(abi.encode(DepositType.Regular, deposit, prevHash))

// Encrypted deposit hash:
keccak256(abi.encode(DepositType.Encrypted, encryptedDeposit, prevHash))
```

This ensures deposits are processed in the exact order they were made, regardless of type.

**Security considerations:**

- **Sequencer trust**: Users trust the sequencer to decrypt correctly and credit the right recipient. A malicious sequencer could steal encrypted deposits.
- **On-chain verification**: The sequencer provides the ECDH shared secret, which enables on-chain decryption verification via GCM tag validation without revealing the private key. See "On-chain decryption verification" below.
- **Key rotation**: The portal maintains a history of encryption keys. Each encrypted deposit includes the `keyIndex` the user encrypted to, allowing the prover to look up the correct key for decryption. See "Encryption key history" below.
- **Malformed ciphertext**: If decryption fails, the sequencer may refund to `sender` or hold funds pending resolution.

**On-chain decryption verification:**

The zone can verify encrypted deposit decryption on-chain without the sequencer revealing their private key. The sequencer provides the ECDH shared secret alongside the decrypted data:

```solidity
struct DecryptionData {
    bytes32 sharedSecret;       // ECDH shared secret (sequencerPriv * ephemeralPub)
    address to;                 // Decrypted recipient
    bytes32 memo;               // Decrypted memo
    ChaumPedersenProof cpProof; // Proof of correct shared secret derivation
}
```

Verification works by leveraging the AES-GCM authentication tag:

1. Sequencer computes: `sharedSecret = ECDH(sequencerPriv, ephemeralPub)`
2. On-chain, derive AES key from `sharedSecret` using HKDF-SHA256
3. Attempt to decrypt the ciphertext with AES-256-GCM
4. **The GCM tag will only validate if the shared secret is correct**
5. If tag validates, the decrypted `(to, memo)` are cryptographically proven authentic

**Griefing attack prevention:**

Without additional checks, a malicious user could submit an encrypted deposit with invalid ciphertext (garbage data or encrypted to the wrong key). The sequencer wouldn't be able to decrypt it, but also couldn't prove it's invalid, blocking chain progress.

**Solution**: Use a **Chaum-Pedersen zero-knowledge proof** to prove the shared secret was correctly derived, without exposing the sequencer's private key to the EVM.

The sequencer provides a Chaum-Pedersen proof that proves: "I know `privSeq` such that `pubSeq = privSeq * G` AND `sharedSecretPoint = privSeq * ephemeralPub`"

This proof allows on-chain verification without revealing the private key:

```solidity
// Step 1: Look up sequencer's public key from on-chain key history
(bytes32 seqPubX, uint8 seqPubYParity) = _readEncryptionKey(ed.keyIndex);

// Step 2: Verify Chaum-Pedersen proof of correct shared secret derivation
bool proofValid = IChaumPedersenVerify(CHAUM_PEDERSEN_VERIFY).verifyProof(
    ed.encrypted.ephemeralPubX,
    ed.encrypted.ephemeralPubYParity,
    dec.sharedSecret,
    seqPubX,          // looked up on-chain, not from dec
    seqPubYParity,    // looked up on-chain, not from dec
    dec.cpProof
);
if (!proofValid) revert InvalidSharedSecretProof();

// Step 3: Derive AES key using HKDF-SHA256 (in Solidity)
// Note: Encryption key validity is already validated on Tempo side in ZonePortal.depositEncrypted()
bytes32 aesKey = _hkdfSha256(dec.sharedSecret, "ecies-aes-key", "");

// Step 4: Try to decrypt using AES-GCM precompile
(bytes memory plaintext, bool valid) = IAesGcmDecrypt(AES_GCM_DECRYPT).decrypt(...);

// Step 5: If decryption fails, return funds to sender (don't block chain)
if (!valid) {
    zoneToken.mint(ed.sender, ed.amount);
    emit EncryptedDepositFailed(...);
}
```

This prevents griefing: users can't block the chain with invalid encryptions, and the sequencer's private key never touches the EVM.

**Chaum-Pedersen proof protocol:**

1. **Prover (sequencer) computes off-chain:**
   - Pick random `r`
   - `R1 = r * G`
   - `R2 = r * ephemeralPub`
   - `c = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)` (Fiat-Shamir challenge)
   - `s = r + c * privSeq (mod n)`
   - Proof is `(s, c)`

2. **Verifier (on-chain) checks:**
   - Reconstruct: `R1 = s*G - c*pubSeq`
   - Reconstruct: `R2 = s*ephemeralPub - c*sharedSecretPoint`
   - Recompute: `c' = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)`
   - Verify: `c == c'`

**Chaum-Pedersen verification precompile** (at `0x1c00000000000000000000000000000000000100`):

```solidity
interface IChaumPedersenVerify {
    function verifyProof(
        bytes32 ephemeralPubX,
        uint8 ephemeralPubYParity,
        bytes32 sharedSecret,
        bytes32 sequencerPubX,
        uint8 sequencerPubYParity,
        ChaumPedersenProof calldata proof
    ) external view returns (bool valid);
}
```

**AES-GCM decryption precompile** (at `0x1c00000000000000000000000000000000000101`):

This is a minimal precompile that only performs AES-256-GCM decryption. HKDF-SHA256 key derivation is implemented in Solidity using the existing SHA256 precompile (0x02), making the precompile simpler and more auditable.

```solidity
interface IAesGcmDecrypt {
    /// @notice Decrypt AES-256-GCM ciphertext and verify authentication tag
    /// @dev Returns empty bytes and false if tag verification fails.
    /// @param key AES-256 key (32 bytes)
    /// @param nonce GCM nonce (12 bytes)
    /// @param ciphertext The encrypted data
    /// @param aad Additional authenticated data (empty for ECIES)
    /// @param tag GCM authentication tag (16 bytes)
    /// @return plaintext The decrypted data (empty if verification fails)
    /// @return valid True if the tag verifies and decryption succeeds
    function decrypt(
        bytes32 key,
        bytes12 nonce,
        bytes calldata ciphertext,
        bytes calldata aad,
        bytes16 tag
    ) external view returns (bytes memory plaintext, bool valid);
}
```

**Key properties:**
- **Zero-knowledge security**: Chaum-Pedersen proof verifies shared secret without exposing sequencer's private key to EVM
- **Griefing resistance**: Invalid encryptions can be detected and rejected, preventing chain blockage
- **Graceful failure**: Invalid encrypted deposits return funds to sender instead of reverting
- **Cryptographic proof**: GCM tag validation proves decryption correctness
- **On-chain verification**: All verification happens on-chain via precompiles
- **Standard crypto**: Uses well-established ECIES, ECDH, Chaum-Pedersen, HKDF-SHA256, and AES-256-GCM

**Precompile implementation:**

- *Chaum-Pedersen Verify* (`0x1c00...0100`): Verifies proof via 2 point multiplications + challenge recomputation (~8000 gas)
- *AES-GCM Decrypt* (`0x1c00...0101`): Symmetric decryption with tag verification (~1000 gas base + ~500/32 bytes)
- *HKDF-SHA256*: Implemented in Solidity using the SHA256 precompile (0x02)

**Encryption key history:**

The portal stores all historical encryption keys. Users specify `keyIndex` at signing time (avoiding key-rotation race conditions). Old keys expire after `ENCRYPTION_KEY_GRACE_PERIOD` (86400 blocks / ~1 day), allowing the sequencer to safely delete old private keys.

Key management: `setSequencerEncryptionKey()`, `encryptionKeyCount()`, `encryptionKeyAt()`, `encryptionKeyAtBlock()`, `isEncryptionKeyValid()`.

## Bridging out (zone to Tempo)

Users withdraw by creating a withdrawal on the zone. Withdrawals are processed in two steps:

1. **Batch submission**: The sequencer calls `finalizeWithdrawalBatch()` at the end of the final block in the batch (even if `count = 0`), which constructs the withdrawal hash and emits a `BatchFinalized` event with the current `withdrawalBatchIndex`. The proof validates `ZoneOutbox.lastBatch()` state and adds the withdrawal hash to Tempo's queue.
2. **Withdrawal processing**: The sequencer calls `processWithdrawal` to process withdrawals from the oldest slot (`head`).

The `withdrawalBatchIndex` ensures batches are submitted in order: each batch's `withdrawalBatchIndex` must match the Tempo portal's expected next batch. This prevents the sequencer from omitting batches that contain withdrawals.

### Withdrawal execution

When the sequencer processes a withdrawal via `processWithdrawal`, the withdrawal is **popped unconditionally** (even on failure). If the transfer or messenger call fails, funds are bounced back via a new deposit.

```solidity
function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external onlySequencer {
    uint256 head = _withdrawalQueue.head;

    // Check if queue is empty
    if (head == _withdrawalQueue.tail) {
        revert NoWithdrawalsInQueue();
    }

    bytes32 currentSlot = _withdrawalQueue.slots[head];

    // Verify head (remainingQueue of 0 means last item, we check against EMPTY_SENTINEL)
    bytes32 expectedRemainingQueue = remainingQueue == bytes32(0) ? EMPTY_SENTINEL : remainingQueue;
    require(keccak256(abi.encode(w, expectedRemainingQueue)) == currentSlot, "invalid");

    // Pop the withdrawal regardless of success/failure
    if (remainingQueue == bytes32(0)) {
        // Slot exhausted, mark as empty and advance head
        _withdrawalQueue.slots[head] = EMPTY_SENTINEL;
        _withdrawalQueue.head = head + 1;
    } else {
        // More withdrawals in this slot
        _withdrawalQueue.slots[head] = remainingQueue;
    }

    if (w.gasLimit == 0) {
        ITIP20(token).transfer(w.to, w.amount);
        return;
    }

    // Try callback via self-call for atomicity
    try this._executeWithdrawal(w) {
        // Success: tokens transferred and callback executed
    } catch {
        // Callback failed: bounce back to zone
        _enqueueBounceBack(w.amount, w.fallbackRecipient);
    }
}
```

The messenger does `token.transferFrom(portal, target, amount)` then executes the callback. Both are atomic: if the callback reverts, the transferFrom reverts too, and funds remain in the portal for bounce-back. Receivers check `msg.sender == messenger` and call `messenger.xDomainMessageSender()` to authenticate the L2 origin. This enables composable withdrawals where funds can flow directly into Tempo contracts (e.g., DEX swaps, staking, cross-zone deposits).

## Withdrawal failure and bounce-back

Withdrawals are popped unconditionally from the queue, so failures never block processing. A withdrawal can fail due to:

- **Transfer failure**: `transfer` or `transferFrom` reverts
- **TIP-403 policy**: Recipient not authorized under the token's transfer policy
- **Token paused**: The zone token is globally paused
- **Callback revert**: The receiver contract reverts (out of gas, logic error, etc.)
- **Callback rejection**: Receiver returns wrong selector

When a withdrawal fails, the portal "bounces back" the funds by enqueuing a new deposit to the withdrawal's `fallbackRecipient` on the same zone. The zone processes this deposit normally and credits the `fallbackRecipient`, so users always retain their funds.

Tempo TIP-20 tokens use TIP-403 for transfer authorization (checks `isAuthorized` on both `from` and `to`). Zone creators SHOULD choose zone tokens with `transferPolicyId == 1` (always-allow) to avoid complexity. If using restricted policies, the portal address MUST be whitelisted and users should set `fallbackRecipient` to an address they control.

## Data availability and liveness

- Zone data availability is fully trusted to the sequencer.
- If the sequencer withholds data or halts, users cannot reconstruct zone state or force exits; batch posting and withdrawal processing are sequencer-only.
- The design assumes users accept this risk in exchange for low-cost and fast settlement.

## Security considerations

- Sequencer can halt the zone without recourse due to missing data availability.
- The verifier is a trust anchor. A faulty verifier can steal or lock funds.
- Withdrawals with callbacks go through the zone messenger with a user-specified gas limit. The messenger does `transferFrom` + callback atomically; any transfer or callback failure triggers a bounce-back to `fallbackRecipient`.
- Deposits are locked on Tempo until a verified batch consumes them.

## Implementation architecture

This section describes the concrete implementation approach for zone nodes.

### Node architecture

Each zone runs as an ExEx (Execution Extension) attached to a Tempo node. There are separate ExEx instances per zone—for example, one ExEx for a USDC zone and another for a USDT zone.

```
┌─────────────────────────────────────────────────────┐
│                  Tempo Node                      │
│  ┌─────────────┐  ┌─────────────┐                   │
│  │ USDC Zone   │  │ USDT Zone   │                   │
│  │   ExEx      │  │   ExEx      │  ...              │
│  └─────────────┘  └─────────────┘                   │
└─────────────────────────────────────────────────────┘
```

### Execution model

- **Payloads**: TIP-20 payloads are submitted via a simple RPC interface (not full reth RPC).
- **TIP-20 precompile**: Payloads are executed through a TIP-20 payments precompile that handles token transfers and fee accounting.
- **revm**: Execution uses revm with custom precompile injections for TIP-20 and payment logic.
- **In-memory backstore**: Zone state is held in an in-memory database for fast access. State is persisted to disk for recovery.

### State commitments

- **Zone block hash**: Computed from the zone block header after execution. The zone block header is a simplified Ethereum header that includes:
  - `parentHash`, `beneficiary`, `stateRoot`, `transactionsRoot`, `receiptsRoot`, `number`, `timestamp`
  - **Omitted fields**: `gasLimit`, `gasUsed` (zones have no hard gas limit), `logsBloom`, `extraData` (not needed for proofs)
- **Transactions/receipts roots**: Computed over the full ordered list `[advanceTempo?, user txs..., finalizeWithdrawalBatch?]`.
- **Transactions root**: Committed in the block hash but not proven on-chain. This prevents sequencer revisionism (claiming different transactions led to the state) while avoiding expensive transaction proof verification.
- **Receipts root**: Committed in the block hash but not proven on-chain. Batch parameters are read from `lastBatch` state storage instead of event logs.
- **Tempo anchoring**: The zone maintains its view of Tempo state via the TempoState predeploy. A zone block may start with a sequencer-only call to `ZoneInbox.advanceTempo()`, which internally calls `TempoState.finalizeTempo()` with the Tempo block header; if omitted, the binding carries over from the previous block. When submitting a batch, the prover specifies a `tempoBlockNumber` and an `anchorBlockNumber`; the proof must demonstrate the zone committed to `tempoBlockNumber` and that the anchor hash matches either the same block (direct mode) or a verified ancestry chain (ancestry mode) ending at `anchorBlockHash` from the EIP-2935 history precompile.

#### Block header field coverage

| Field | In Hash | Proven | How verified |
|-------|---------|--------|--------------|
| `parentHash` | ✓ | ✓ | Portal checks `prevBlockHash == blockHash`; proof validates chain continuity |
| `beneficiary` | ✓ | ✓ | Proof validates beneficiary matches the registered sequencer address |
| `stateRoot` | ✓ | ✓ | Core of proof; `lastBatch` and other state reads validated against this |
| `transactionsRoot` | ✓ | ✗ | Committed but not proven on-chain; prevents sequencer revisionism |
| `receiptsRoot` | ✓ | ✗ | Committed but not proven on-chain; batch params read from state instead |
| `number` | ✓ | ✓ | Proof validates block number as part of the header transition |
| `timestamp` | ✓ | ✓ | Proof validates timestamp is monotonically increasing from previous block |
| `gasLimit` | ✗ | N/A | Omitted — zones have no hard gas limit |
| `gasUsed` | ✗ | N/A | Omitted — zones have no hard gas limit |
| `logsBloom` | ✗ | N/A | Omitted — not needed for proofs |
| `extraData` | ✗ | N/A | Omitted — not needed for proofs |

### Batching and proofs

- **Batch interval**: Batches are produced every 250 milliseconds.
- **SP1 proofs**: Validity proofs are generated using Succinct's SP1 prover.
- **Mock proofs**: For development, proofs are mocked but data structures (public inputs, proof envelope) must match the real format.
- **Sequencer posting only**: Only the configured sequencer posts batch proofs to the Tempo portal. The proof includes block hash and processed deposits.

```solidity
struct BatchProof {
    bytes32 nextBlockHash;
    uint64 withdrawalBatchIndex;            // withdrawal batch index from ZoneOutbox.lastBatch (must equal portal.withdrawalBatchIndex + 1)
    uint64 tempoBlockNumber;      // Tempo block the zone synced to (must equal TempoState.tempoBlockNumber)
    bytes32 withdrawalQueueHash;  // hash chain of withdrawals for this batch (0 if none)
    bytes verifierConfig;         // opaque payload to IVerifier (TEE/ZK envelope)
    bytes proof;                  // SP1 proof bytes (or TEE attestation)
}
```
The portal provides `blockHash` and `withdrawalBatchIndex` as the previous batch's values. The proof reads `withdrawalBatchIndex` and `withdrawalQueueHash` from `ZoneOutbox.lastBatch()` state storage, and validates that `TempoState.tempoBlockHash()` and `TempoState.tempoBlockNumber()` match the EIP-2935 history precompile value and `tempoBlockNumber`.

### Deposits and withdrawals

- **Deposit contract**: Tempo portal escrows TIP-20 tokens. The ExEx watches `DepositMade` events and queues deposits for zone processing.
- **Combined sequencer call**: A zone block may start with a sequencer-only call to `ZoneInbox.advanceTempo(header, deposits)`. This atomically advances the zone's Tempo view and processes pending deposits, validating the deposit hash against Tempo state. If omitted, no deposits are processed and the Tempo binding is unchanged for that block.
- **Withdrawal requests**: Users trigger withdrawals on the zone via RPC. The withdrawal is added to the pending exits and included in the next batch's exit list.

### RPC interface

The zone exposes a minimal RPC (not full reth JSON-RPC):

```
zone_sendPayload(payload) -> txHash
zone_requestWithdrawal(recipient, amount) -> withdrawalId
zone_getState(address) -> balance
zone_getReceipt(txHash) -> receipt
```

### Multi-zone ExEx structure

```
Tempo Node
├── ExEx: USDC Zone
│   ├── TIP-20 Precompile (USDC)
│   ├── Payments Precompile
│   ├── In-memory State Store
│   └── SP1 Prover (mock for dev)
│
└── ExEx: USDT Zone
    ├── TIP-20 Precompile (USDT)
    ├── Payments Precompile
    ├── In-memory State Store
    └── SP1 Prover (mock for dev)
```

## Open questions

- Should deposits be cancellable if not consumed within a timeout?
