# Tempo Zones (Draft)

This document proposes a new validium protocol designed for Tempo. It is a design overview, not a full specification.

## Goals

- Create a Tempo-native validium called a zone.
- Each zone has exactly one permissioned sequencer.
- Each zone supports multiple TIP-20 tokens for bridging. The sequencer can enable additional TIP-20 tokens at any time. All enabled tokens are usable as gas tokens on the zone (no feeAMMs).
- Settlement uses fast validity proofs or TEE attestations (ZK or TEE). Data availability is fully trusted to the sequencer.
- Cross-chain operations are Tempo-centric: bridge in (simple deposit), bridge out (with optional callback to receiver contracts for Tempo composability).
- Verifier is abstracted behind a minimal `IVerifier` interface.
- Liveness (including exits) is wholly dependent on the permissioned sequencer; there is no permissionless fallback.
- Non-custodial withdrawal guarantee: once a token is enabled, withdrawals for that token can never be disabled.

## Non-goals

- No attempt to solve data availability, forced inclusion, or censorship resistance.
- No upgradeability or governance model.
- No general messaging. Multi-asset bridging is supported for all TIP-20 tokens enabled by the sequencer.

## Terminology

- Tempo: the base chain.
- Zone: the validium chain anchored to Tempo.
- Enabled tokens: TIP-20 tokens that the sequencer has enabled for bridging in/out. Each zone starts with an initial token and the sequencer can enable more. Token enablement is permanent (append-only).
- Portal: the Tempo-side contract that escrows enabled tokens and finalizes exits.
- Batch: a sequencer-produced commitment covering one or more zone blocks. The batch **must** end with a single `finalizeWithdrawalBatch()` call in the final block, and intermediate blocks **must not** call `finalizeWithdrawalBatch()`. The sequencer controls batch frequency.

## System overview

### Actors

- Zone sequencer: permissioned operator that orders zone transactions, provides data, and posts batches to Tempo. The sequencer is the only actor that submits transactions to the portal.
- Verifier: ZK proof system or TEE attester. Abstracted via `IVerifier`.
- Users: deposit TIP-20 from Tempo to the zone or exit back to Tempo.

### Tempo contracts

- `ZoneFactory`: creates zones and registers parameters.
- `ZonePortal`: per-zone portal that escrows enabled tokens on Tempo and finalizes exits. Manages the token registry.

### Zone components (off-chain or zone-side)

- `ZoneSequencer`: collects transactions and creates batches.
- `ZoneExecutor`: executes the zone state transition.
- `ZoneProver` or `ZoneAttester`: produces proof/attestation for each batch.

## Zone creation

A zone is created via `ZoneFactory.createZone(...)` with:

- `initialToken`: the first Tempo TIP-20 address to enable. The sequencer can enable additional tokens later via `ZonePortal.enableToken()`.
- `sequencer`: permissioned sequencer address.
- `verifier`: `IVerifier` implementation for proof or attestation.
- `zoneParams`: initial configuration (genesis block hash, genesis Tempo block hash/number).

The factory deploys a `ZonePortal` that escrows enabled tokens on Tempo and a `ZoneMessenger` for withdrawal callbacks. The initial token is automatically enabled at deployment.

### Token management

The sequencer manages which tokens are available for bridging:

- `enableToken(address token)`: Enable a new TIP-20 for bridging. **Irreversible** — once enabled, a token can never be disabled.
- `pauseDeposits(address token)`: Pause new deposits for a token (sequencer-only). Does not affect withdrawals.
- `resumeDeposits(address token)`: Resume deposits for a previously paused token.

The portal maintains a `TokenConfig` per token with `enabled` (permanent) and `depositsActive` (toggleable) flags, and an append-only `enabledTokens` list for enumeration. This design enforces the **non-custodial withdrawal guarantee**: the sequencer can halt deposits but can never prevent users from withdrawing an enabled token.

### Sequencer transfer

The sequencer can transfer control to a new address via a two-step process on **Tempo L1 only**:

1. Current sequencer calls `ZonePortal.transferSequencer(newSequencer)` to nominate a new sequencer
2. New sequencer calls `ZonePortal.acceptSequencer()` to accept the transfer

**Sequencer management is centralized on L1 (Tempo).** Zone-side system contracts (`ZoneInbox`, `ZoneOutbox`) read the sequencer from L1 via `ZoneConfig`, which queries `TempoState` to get the sequencer address from the finalized `ZonePortal` storage. This eliminates duplicate sequencer management logic and ensures L1 is the single source of truth. The two-step pattern prevents accidental transfers to incorrect addresses.

## Execution and fees

- The zone reuses Tempo's fee units and accounting model.
- Zone transactions specify which enabled TIP-20 token to use for gas fees via a `feeToken` field. The sequencer is required to accept all enabled tokens as gas (no feeAMMs on the zone).
- Transactions use Tempo transaction semantics for fee payer, max fee per gas, and gas limit.

### Deposit fees

Deposits incur a processing fee to compensate the sequencer for zone-side processing costs:

- **Zone gas rate**: Sequencer publishes `zoneGasRate` (token units per gas unit)
- **Fixed gas value**: `FIXED_DEPOSIT_GAS` is fixed at 100,000 gas
- **Total fee**: `FIXED_DEPOSIT_GAS * zoneGasRate` = `100,000 * zoneGasRate`

The fee is paid in the **same token being deposited**. The sequencer configures `zoneGasRate` via `ZonePortal.setZoneGasRate()`. The fixed gas value of 100,000 provides a stable pricing basis for deposits while allowing the sequencer flexibility to adjust the rate based on operational costs and future deposit mechanism variations. A single uniform `zoneGasRate` applies to all tokens (stablecoins of the same value).

The fee is deducted from the deposit amount and paid to the sequencer immediately on Tempo. The deposit queue stores the net amount (`amount - fee`) which is minted on the zone.

### Withdrawal processing fees

Withdrawals incur a processing fee to compensate the sequencer for Tempo-side gas costs:

- **Tempo gas rate**: Sequencer publishes `tempoGasRate` (token units per gas unit)
- **Gas limit**: User specifies `gasLimit` covering all execution costs (processing + callback)
- **Total fee**: `gasLimit * tempoGasRate`

The fee is paid in the **same token being withdrawn**. The user must estimate total gas needed for their withdrawal, including `processWithdrawal` overhead and any callback. The sequencer configures `tempoGasRate` via `ZoneOutbox.setTempoGasRate()` and takes the risk on Tempo gas price fluctuations. If actual Tempo gas is higher, the sequencer covers the difference; if lower, they keep the surplus.

Users burn `amount + fee` of the specified token when requesting a withdrawal. On success, `amount` goes to the recipient and `fee` goes to the sequencer. On failure (bounce-back), only `amount` is re-deposited to `fallbackRecipient`; the sequencer keeps the fee.

## Batch submission

The sequencer posts batches to Tempo via a single `submitBatch` call (sequencer-only) that:

1. Verifies the proof/attestation for the state transition (including chain integrity via `prevBlockHash`).
2. Updates the portal's `withdrawalBatchIndex`, `blockHash`, and `lastSyncedTempoBlockNumber`.
3. Updates the withdrawal queue (adds new withdrawals to the next slot in the fixed-size ring buffer).

Each batch submission includes:

- `tempoBlockNumber` - Block zone committed to (from zone's TempoState)
- `recentTempoBlockNumber` - Optional recent block for ancestry proof (0 = direct lookup)
- `blockTransition` - Zone block hash transition (prevBlockHash → nextBlockHash)
- `depositQueueTransition` - Deposit queue processing (prevProcessedHash → nextProcessedHash)
- `withdrawalQueueTransition` - Withdrawal queue hash (hash chain for this batch, or 0 if none)
- `verifierConfig` - Opaque payload for verifier (domain separation/attestation)
- `proof` - Validity proof or TEE attestation

The portal tracks `withdrawalBatchIndex`, `blockHash` (last proven batch block), `lastSyncedTempoBlockNumber` (Tempo block zone synced to), `currentDepositQueueHash` (deposit queue head), and a fixed-size ring buffer for withdrawals.

**Ancestry proofs for historical blocks**: If `tempoBlockNumber` is outside the EIP-2935 window (~8192 blocks), use `recentTempoBlockNumber` to specify a recent block still in EIP-2935. The proof verifies the ancestry chain (via parent hashes) from `tempoBlockNumber` to `recentTempoBlockNumber` inside the ZK proof, avoiding expensive on-chain header verification. This prevents zone bricking after extended downtime.

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
///      that nextProcessedHash matches currentDepositQueueHash for now. 
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

**Proof requirements**: The proof validates deposit processing by reading `currentDepositQueueHash` from Tempo state inside the proof. The zone's `advanceTempo()` function processes deposits and updates the zone's `processedDepositQueueHash`. The proof ensures this was done correctly by validating the Tempo state read. For now, the on-chain inbox requires an exact match.

**After batch accepted**:
1. `lastSyncedTempoBlockNumber = tempoBlockNumber` (record how far Tempo state was synced)

New deposits continue to land in `currentDepositQueueHash` while proofs are in flight. Users can check if their deposit is processed by comparing their deposit's Tempo block number against `lastSyncedTempoBlockNumber`.

Proofs or attestations are assumed to be fast. No data availability is required by the verifier.

## Withdrawal queue

Withdrawals use a fixed-size ring buffer (capacity `WITHDRAWAL_QUEUE_CAPACITY = 100`) that allows the sequencer to process withdrawals independently of proof generation. Each batch gets its own slot, and the sequencer processes withdrawals from the oldest slot while new batches add to the next available slot. Head and tail are raw counters that never wrap; modular arithmetic (`index % WITHDRAWAL_QUEUE_CAPACITY`) is used only for slot indexing.

The portal tracks:
- `head` - slot index of the oldest unprocessed batch (where sequencer removes)
- `tail` - slot index where the next batch will write (where proofs add)
- `slots` - mapping of slot index to hash chain (`EMPTY_SENTINEL` = empty)

The queue reverts with `WithdrawalQueueFull` if `tail - head >= WITHDRAWAL_QUEUE_CAPACITY` (i.e., all 100 slots are occupied).

### Hash chain structure

Each slot contains a hash chain with the **oldest withdrawal at the outermost layer**, making FIFO processing efficient. The innermost element wraps `EMPTY_SENTINEL` (0xffffffff...fff) instead of 0x00 to avoid clearing storage:

```
slot = keccak256(abi.encode(w1, keccak256(abi.encode(w2, keccak256(abi.encode(w3, EMPTY_SENTINEL))))))
      // w1 is oldest (outermost), w3 is newest (innermost)
```

To process the oldest withdrawal, the sequencer provides the withdrawal data and the remaining queue hash. The portal verifies the hash and updates the slot:

```solidity
function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external onlySequencer {
    uint256 head = _withdrawalQueue.head;

    // Check if queue is empty
    if (head == _withdrawalQueue.tail) {
        revert NoWithdrawalsInQueue();
    }

    uint256 slotIndex = head % WITHDRAWAL_QUEUE_CAPACITY;
    bytes32 currentSlot = _withdrawalQueue.slots[slotIndex];

    // Verify this is the head of the current slot
    // The remainingQueue for the last item should be 0 (we convert to EMPTY_SENTINEL internally)
    bytes32 expectedRemainingQueue = remainingQueue == bytes32(0) ? EMPTY_SENTINEL : remainingQueue;
    require(keccak256(abi.encode(w, expectedRemainingQueue)) == currentSlot, "invalid");

    _executeWithdrawal(w);

    if (remainingQueue == bytes32(0)) {
        // Slot exhausted, mark as empty and advance head
        _withdrawalQueue.slots[slotIndex] = EMPTY_SENTINEL;
        _withdrawalQueue.head = head + 1;
    } else {
        // More withdrawals in this slot
        _withdrawalQueue.slots[slotIndex] = remainingQueue;
    }
}
```

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

    // Revert if all slots are occupied
    if (tail - _withdrawalQueue.head >= WITHDRAWAL_QUEUE_CAPACITY) {
        revert WithdrawalQueueFull();
    }

    // Write the withdrawal hash chain to this slot (modular indexing)
    _withdrawalQueue.slots[tail % WITHDRAWAL_QUEUE_CAPACITY] = withdrawalQueueTransition.withdrawalQueueHash;

    // Advance tail
    _withdrawalQueue.tail = tail + 1;
}
```

This design eliminates race conditions entirely - each batch has its own independent slot, and the sequencer processes slots in order. The fixed-size ring buffer (capacity 100) bounds storage usage while providing ample room for normal operation.

## Interfaces and functions

This section defines the functions and interfaces used by the design. The signatures are Solidity-style and focus on the minimum surface area.

### Common types

```solidity
/// @notice Interface for the zone's zone token (TIP-20 with mint/burn for system)
interface IZoneToken {
    function mint(address to, uint256 amount) external;
    function burn(address from, uint256 amount) external;
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

struct ZoneInfo {
    uint64 zoneId;
    address portal;
    address messenger;
    address initialToken;       // first TIP-20 enabled at creation
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

/// @notice Per-token configuration in the portal's token registry
struct TokenConfig {
    bool enabled;          // true once sequencer enables this token (permanent)
    bool depositsActive;   // sequencer can pause/unpause deposits
}

struct Deposit {
    address token;          // TIP-20 token being deposited
    address sender;
    address to;
    uint128 amount;
    bytes32 memo;
}

struct Withdrawal {
    address token;              // TIP-20 token being withdrawn
    bytes32 senderTag;          // keccak256(abi.encodePacked(sender, txHash)) — see Authenticated withdrawals
    address to;                 // Tempo recipient
    uint128 amount;             // amount to send to recipient (excludes fee)
    uint128 fee;                // processing fee for sequencer (calculated at request time)
    bytes32 memo;
    uint64 gasLimit;            // max gas for IWithdrawalReceiver callback (0 = no callback)
    address fallbackRecipient;  // zone address for bounce-back if call fails
    bytes callbackData;         // calldata for IWithdrawalReceiver (if gasLimit > 0, max 1KB)
    bytes encryptedSender;      // ECDH-encrypted (sender, txHash) for revealTo key, or empty
}
```

### Verifier

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

The verifier receives `tempoBlockNumber`, `anchorBlockNumber`, and `anchorBlockHash` (looked up on-chain via the EIP-2935 block hash history precompile), `expectedWithdrawalBatchIndex` (portal's current batch index + 1), `sequencer` (the registered sequencer address), block transition, deposit queue transition, withdrawal queue transition, and the proof. The proof must demonstrate that the zone committed to `tempoBlockNumber` via TempoState, that the anchor hash is correct (direct or ancestry mode), that the state transition is valid, that `ZoneOutbox.lastBatch().withdrawalBatchIndex` equals `expectedWithdrawalBatchIndex`, that the zone block `beneficiary` matches `sequencer`, and that `ZoneOutbox.lastBatch().withdrawalQueueHash` matches `withdrawalQueueTransition.withdrawalQueueHash` (read from state root, not events).

### Queue libraries

The portal uses two queue libraries that encapsulate the hash chain management patterns:

#### DepositQueueLib

Handles Tempo→L2 deposits. The Tempo portal only tracks `currentDepositQueueHash` (the head of the queue where new deposits land). The zone tracks its own `processedDepositQueueHash` in EVM state, and the proof validates deposit processing by reading `currentDepositQueueHash` from Tempo state.

```solidity
library DepositQueueLib {
    /// @notice Enqueue a new deposit into the queue (on-chain operation)
    /// @dev Hash chain: newHash = keccak256(abi.encode(deposit, prevHash))
    /// @param currentHash The current head of the deposit queue
    /// @param depositData The deposit to enqueue
    /// @return newHash The new head of the deposit queue
    function enqueue(bytes32 currentHash, Deposit memory depositData) internal pure returns (bytes32 newHash);
}
```

#### WithdrawalQueueLib (fixed-size ring buffer)

Handles L2→Tempo withdrawals where the producer (proof) is slow and the consumer (on-chain) is fast. Each batch gets its own slot in a fixed-size ring buffer with capacity `WITHDRAWAL_QUEUE_CAPACITY = 100`.

```solidity
/// @dev Sentinel value for empty slots. Using 0xff...ff instead of 0x00 to avoid
///      clearing storage (which would refund gas and create incentive issues).
bytes32 constant EMPTY_SENTINEL = bytes32(type(uint256).max);


uint256 constant WITHDRAWAL_QUEUE_CAPACITY = 100;

struct WithdrawalQueue {
    uint256 head;     // logical index of oldest unprocessed batch
    uint256 tail;     // logical index where next batch will write
    mapping(uint256 => bytes32) slots;  // hash chains per batch (EMPTY_SENTINEL = empty)
}

library WithdrawalQueueLib {
    /// @notice Add a batch's withdrawals to the queue (called during batch submission)
    /// @dev Writes to slot at tail % WITHDRAWAL_QUEUE_CAPACITY, then advances tail.
    ///      Reverts with WithdrawalQueueFull if tail - head >= WITHDRAWAL_QUEUE_CAPACITY.
    function enqueue(WithdrawalQueue storage q, WithdrawalQueueTransition memory transition) internal;

    /// @notice Pop the next withdrawal from the queue (on-chain operation)
    /// @dev Verifies the withdrawal is at the head of the current slot and advances.
    ///      When a slot is exhausted, it's set to EMPTY_SENTINEL and head advances.
    function dequeue(WithdrawalQueue storage q, Withdrawal calldata w, bytes32 remainingQueue) internal;

    /// @notice Check if the queue has any pending withdrawals
    function hasWithdrawals(WithdrawalQueue storage q) internal view returns (bool);

    /// @notice Get current queue length
    function length(WithdrawalQueue storage q) internal view returns (uint256);
}
```

**Gas note**: The ring buffer reuses slots via modular indexing, so storage writes to already-occupied slots cost less gas (warm storage). New storage charges only apply when the queue grows into a slot that has never been used.


| Queue | Tempo operation | Zone/Proof operation |
|-------|--------------|---------------------|
| Deposit | `enqueue` (users deposit) | Process via `advanceTempo()` |
| Withdrawal | `dequeue` (sequencer processes) | Create via `finalizeWithdrawalBatch()` |

### Tempo contracts

#### Zone factory

```solidity
interface IZoneFactory {
    struct CreateZoneParams {
        address initialToken;   // first TIP-20 to enable (sequencer can enable more later)
        address sequencer;
        address verifier;
        ZoneParams zoneParams;
    }

    event ZoneCreated(
        uint64 indexed zoneId,
        address indexed portal,
        address indexed messenger,
        address initialToken,
        address sequencer,
        address verifier,
        bytes32 genesisBlockHash,
        bytes32 genesisTempoBlockHash,
        uint64 genesisTempoBlockNumber
    );

    function createZone(CreateZoneParams calldata params) external returns (uint64 zoneId, address portal);
    function zoneCount() external view returns (uint64);
    function zones(uint64 zoneId) external view returns (ZoneInfo memory);
    function isZonePortal(address portal) external view returns (bool);
}
```

#### Zone portal

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

    /// @notice Emitted when an encrypted deposit is made (recipient/memo not revealed)
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

    /// @notice Emitted when sequencer updates their encryption key
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

    /// @notice Fixed gas value for deposit fee calculation (100,000 gas).
    function FIXED_DEPOSIT_GAS() external view returns (uint64);

    function zoneId() external view returns (uint64);
    function messenger() external view returns (address);
    function sequencer() external view returns (address);
    function pendingSequencer() external view returns (address);
    function zoneGasRate() external view returns (uint128);
    function verifier() external view returns (address);
    function genesisTempoBlockNumber() external view returns (uint64);

    /// @notice Token registry (multi-asset support)
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

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    function transferSequencer(address newSequencer) external;

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external;

    /// @notice Set zone gas rate. Only callable by sequencer.
    function setZoneGasRate(uint128 _zoneGasRate) external;

    /// @notice Calculate the fee for a deposit.
    function calculateDepositFee() external view returns (uint128 fee);

    /// @notice Deposit a TIP-20 token into the zone. Fee is deducted from amount.
    function deposit(address token, address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositQueueHash);

    /// @notice Deposit with encrypted recipient and memo. Fee is deducted from amount.
    function depositEncrypted(
        address token,
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata encrypted
    ) external returns (bytes32 newCurrentDepositQueueHash);

    /// @notice Get the current sequencer encryption key.
    /// @dev Reverts with NoEncryptionKeySet() if no key has been set.
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);

    /// @notice Set sequencer encryption key with proof of possession.
    /// @dev Requires ECDSA signature proving control of the corresponding private key.
    function setSequencerEncryptionKey(
        bytes32 x, uint8 yParity, uint8 popV, bytes32 popR, bytes32 popS
    ) external;

    /// @notice Number of encryption keys in history.
    function encryptionKeyCount() external view returns (uint256);

    /// @notice Get a historical encryption key entry by index.
    function encryptionKeyAt(uint256 index) external view returns (EncryptionKeyEntry memory);

    /// @notice Get the encryption key active at a specific block.
    /// @dev Reverts with NoEncryptionKeyAtBlock() if no key was active at that block.
    function encryptionKeyAtBlock(uint64 tempoBlockNumber)
        external view returns (bytes32 x, uint8 yParity, uint256 keyIndex);

    /// @notice Check if an encryption key is still valid for new deposits.
    function isEncryptionKeyValid(uint256 keyIndex)
        external view returns (bool valid, uint64 expiresAtBlock);

    /// @notice Process the next withdrawal from the queue. Only callable by the sequencer.
    function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external;

    /// @notice Submit a batch and verify the proof. Only callable by the sequencer.
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

#### Zone messenger (Tempo)

Each zone has a dedicated messenger contract on Tempo. The portal gives the messenger max approval for each enabled token (granted when `enableToken()` is called). Withdrawal callbacks originate from this contract, not the portal.

```solidity
interface IZoneMessenger {
    /// @notice Returns the zone's portal address
    function portal() external view returns (address);

    /// @notice Relay a withdrawal message. Only callable by the portal.
    /// @dev Transfers tokens from portal to target via transferFrom, then executes callback.
    ///      If callback reverts, the entire call reverts (including the transfer).
    /// @param token The TIP-20 token to transfer
    /// @param sender The L2 origin address
    /// @param target The Tempo recipient
    /// @param amount Tokens to transfer from portal to target
    /// @param gasLimit Max gas for the callback
    /// @param data Calldata for the target
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

The messenger does `ITIP20(token).transferFrom(portal, target, amount)` then calls the target with `data`. Both are atomic: if the callback reverts, the transfer is also reverted. Receivers check `msg.sender == zoneMessenger` to authenticate the call, and receive the `senderTag` in `onWithdrawalReceived` (see [Authenticated withdrawals](#authenticated-withdrawals)). This enables composable withdrawals where funds can flow directly into Tempo contracts (e.g., DEX swaps, staking, cross-zone deposits).

#### Withdrawal receiver

Contracts that receive withdrawals with callbacks must implement this interface:

```solidity
interface IWithdrawalReceiver {
    /// @notice Called when a withdrawal with callback is received
    /// @param sender The L2 origin address
    /// @param token The TIP-20 token transferred
    /// @param amount The amount of tokens transferred
    /// @param callbackData The callback data from the withdrawal request
    /// @return The function selector to confirm successful handling
    function onWithdrawalReceived(
        bytes32 senderTag,
        address token,
        uint128 amount,
        bytes calldata callbackData
    ) external returns (bytes4);
}
```

The receiver must return `IWithdrawalReceiver.onWithdrawalReceived.selector` to confirm successful handling. If the receiver reverts or returns the wrong selector, the withdrawal fails and bounces back.

### Zone predeploys

Zones have four system contract predeploys at fixed addresses:

- **TempoState** (0x1c00000000000000000000000000000000000000) - Stores finalized Tempo state and provides storage read access
- **ZoneInbox** (0x1c00000000000000000000000000000000000001) - Advances Tempo state and processes deposits
- **ZoneOutbox** (0x1c00000000000000000000000000000000000002) - Handles withdrawal requests back to Tempo
- **ZoneConfig** (0x1c00000000000000000000000000000000000003) - Central configuration that reads sequencer from L1

#### Zone tokens

Each enabled TIP-20 token is bridged from Tempo to the zone. Each is deployed at the **same address** on the zone as on Tempo. Users interact with them via the standard TIP-20 interface for transfers and approvals. The zone sequencer mints the correct token when processing deposits and burns the correct token when withdrawals are requested. The zone node must deploy/configure zone-side representations for each enabled token.

#### ZoneConfig predeploy

ZoneConfig is the central zone configuration contract that provides access to zone metadata and reads the sequencer from L1. It is deployed at **0x1c00000000000000000000000000000000000003**.

```solidity
interface IZoneConfig {
    error NotSequencer();
    error NoEncryptionKeySet();

    /// @notice L1 ZonePortal address
    function tempoPortal() external view returns (address);

    /// @notice TempoState predeploy for L1 reads
    function tempoState() external view returns (ITempoState);

    /// @notice Get current sequencer by reading from L1 ZonePortal
    function sequencer() external view returns (address);

    /// @notice Get pending sequencer by reading from L1 ZonePortal
    function pendingSequencer() external view returns (address);

    /// @notice Get sequencer's encryption public key by reading from L1 ZonePortal
    /// @dev Reverts with NoEncryptionKeySet() if no key has been set.
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);

    /// @notice Check if an address is the current sequencer
    function isSequencer(address account) external view returns (bool);

    /// @notice Check if a token is enabled by reading from L1 ZonePortal
    function isEnabledToken(address token) external view returns (bool);
}
```

ZoneConfig reads the sequencer address and token registry from the finalized L1 `ZonePortal` storage via `TempoState.readTempoStorageSlot()`. This makes L1 the **single source of truth** for sequencer management and token enablement, eliminating duplicate logic on zone-side contracts. Zone system contracts (`ZoneInbox`, `ZoneOutbox`) reference `ZoneConfig` for sequencer checks.

#### TempoState predeploy

The TempoState predeploy allows zones to verify they have a correct view of Tempo state. It stores the Tempo wrapper fields and selected inner Ethereum header fields, and provides storage reading functionality for system contracts. Deployed at **0x1c00000000000000000000000000000000000000**.

```solidity
interface ITempoState {
    event TempoBlockFinalized(bytes32 indexed blockHash, uint64 indexed blockNumber, bytes32 stateRoot);

    /// @notice Current finalized Tempo block hash (keccak256 of RLP-encoded header)
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

    /// @notice Finalize a Tempo block header. Only callable by ZoneInbox.
    /// @dev Validates chain continuity (parent hash must match, number must be +1).
    ///      Called by ZoneInbox.advanceTempo(). Executor enforces ZoneInbox-only access.
    /// @param header RLP-encoded Tempo header
    function finalizeTempo(bytes calldata header) external;

    /// @notice Read a storage slot from a Tempo contract
    /// @dev RESTRICTED: Only callable by zone system contracts (ZoneInbox, ZoneOutbox, ZoneConfig).
    ///      Used to read ZonePortal and TIP-403 policy state from Tempo.
    function readTempoStorageSlot(address account, bytes32 slot) external view returns (bytes32);

    /// @notice Read multiple storage slots from a Tempo contract
    /// @dev RESTRICTED: Only callable by zone system contracts (ZoneInbox, ZoneOutbox, ZoneConfig).
    function readTempoStorageSlots(address account, bytes32[] calldata slots) external view returns (bytes32[] memory);
}
```

Tempo headers are RLP encoded as `rlp([general_gas_limit, shared_gas_limit, timestamp_millis_part, inner])`, where `inner` is a standard Ethereum header. The inner header uses trailing-field semantics for optional fields: `baseFeePerGas` (EIP-1559), `withdrawalsRoot` (EIP-4895), `blobGasUsed`/`excessBlobGas` (EIP-4844), `parentBeaconBlockRoot` (EIP-4788), and `requestsHash` (EIP-7685). TempoState skips these trailing optional fields and does not expose them.

The TempoState stores the Tempo wrapper fields and the inner fields needed by the zone/proof logic; the `tempoBlockHash` is always `keccak256(RLP(TempoHeader))`, so proofs still commit to the complete header contents.

**How it works:**

1. ZoneInbox calls `finalizeTempo()` when the sequencer chooses to advance Tempo for a block, which decodes the RLP header, validates chain continuity, and stores the wrapper fields and selected inner fields. If a block omits `advanceTempo`, the Tempo binding carries over from the previous block.
2. When submitting a batch, the prover specifies a `tempoBlockNumber` and optionally a `recentTempoBlockNumber`. The portal reads the hash for `anchorBlockNumber` via the EIP-2935 block hash history precompile.
3. The proof must demonstrate that the zone's `tempoBlockHash` (from TempoState) matches the portal's `anchorBlockHash` in direct mode, or that the parent-hash chain from `tempoBlockNumber` reaches `anchorBlockHash` in ancestry mode.
4. The `readTempoStorageSlot` functions are **precompile stubs restricted to system contracts only** - actual implementation is in the zone node, validated against `tempoStateRoot`. Only ZoneInbox (0x1c00...0001), ZoneOutbox (0x1c00...0002), and ZoneConfig (0x1c00...0003) can call these functions. User transactions cannot directly read Tempo state.

Tempo state staleness depends on how frequently the sequencer updates tempo state using `advanceTempo()`. The zone client must only finalize Tempo headers after finality; proofs should only reference finalized Tempo blocks to avoid reorg risk. The prover includes Merkle proofs for each unique account and storage slot accessed by system contracts during the batch.

#### TIP-403 registry

The zone has a `TIP403Registry` contract deployed at the **same address** as Tempo. This contract is read-only—it and does not support writing policies. Its `isAuthorized` function reads policy state from Tempo via the Tempo state reader precompile, so zone-side TIP-20 transfers enforce Tempo TIP-403 policies automatically.

#### Zone inbox

The zone inbox is a system contract that advances Tempo state and processes deposits from Tempo in a single atomic operation. It is called by the sequencer at the start of a block when Tempo is advanced; blocks may omit this call and carry forward the existing Tempo binding.

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

    /// @notice Emitted when an encrypted deposit is processed (decrypted and credited)
    event EncryptedDepositProcessed(
        bytes32 indexed depositHash,
        address indexed sender,
        address indexed to,
        address token,
        uint128 amount,
        bytes32 memo
    );

    /// @notice Emitted when an encrypted deposit fails (invalid ciphertext, funds returned to sender)
    event EncryptedDepositFailed(
        bytes32 indexed depositHash, address indexed sender, address token, uint128 amount
    );

    error OnlySequencer();
    error InvalidDepositQueueHash();
    error MissingDecryptionData();
    error ExtraDecryptionData();
    error InvalidSharedSecretProof();

    /// @notice Zone configuration (reads sequencer from L1)
    function config() external view returns (IZoneConfig);

    /// @notice The Tempo portal address (for reading deposit queue hash).
    function tempoPortal() external view returns (address);

    /// @notice The TempoState predeploy address.
    function tempoState() external view returns (ITempoState);

    /// @notice The zone's last processed deposit queue hash.
    function processedDepositQueueHash() external view returns (bytes32);

    /// @notice Advance Tempo state and process deposits in a single sequencer-only call.
    /// @dev This is the main entry point for the sequencer at block start.
    ///      1. Advances the zone's view of Tempo by processing the header
    ///      2. Processes deposits from the unified deposit queue (regular + encrypted)
    ///      3. Validates the resulting hash against Tempo's currentDepositQueueHash
    /// @param header RLP-encoded Tempo block header
    /// @param deposits Array of queued deposits to process (oldest first, must be contiguous)
    /// @param decryptions Decryption data for encrypted deposits (1:1 with encrypted deposits, in order)
    function advanceTempo(
        bytes calldata header,
        QueuedDeposit[] calldata deposits,
        DecryptionData[] calldata decryptions
    ) external;
}
```

The sequencer observes `DepositMade` events on the Tempo portal and relays them to the zone via `advanceTempo`. This function:

1. Calls `TempoState.finalizeTempo(header)` to advance the zone's view of Tempo
2. Processes deposits in order, building the hash chain and minting the correct zone-side TIP-20 tokens
3. Reads `currentDepositQueueHash` from the Tempo portal's storage via `TempoState.readTempoStorageSlot()`
4. Validates the resulting hash matches Tempo's current state

This combined approach ensures Tempo state advancement and deposit processing are atomic, and the deposit hash is validated against the actual Tempo state at the newly finalized block.

#### Zone outbox

The zone outbox handles withdrawal requests. Users approve the outbox to spend their tokens, then call `requestWithdrawal(token, ...)` specifying which TIP-20 to withdraw. The outbox stores pending withdrawals in an array. When the sequencer is ready to finalize a **batch**, it calls `finalizeWithdrawalBatch(count)` at the end of the **final block** in that batch. This constructs the withdrawal queue hash on-chain and writes the `withdrawalQueueHash` and `withdrawalBatchIndex` to storage. Intermediate blocks **must not** call `finalizeWithdrawalBatch()`. The call is required even if there are zero withdrawals (use `count = 0`) so the withdrawal batch index advances. The event is emitted for observability, but the proof reads from state (via the `lastBatch` storage) rather than parsing event logs.

```solidity
/// @notice Withdrawal batch parameters stored in state for proof access
struct LastBatch {
    bytes32 withdrawalQueueHash;
    uint64 withdrawalBatchIndex;
}

interface IZoneOutbox {
    /// @notice Maximum callback data size (1KB)
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

    /// @notice Emitted when sequencer finalizes a batch at end of block.
    /// @dev Kept for observability. Proof reads from lastBatch storage instead.
    event BatchFinalized(
        bytes32 indexed withdrawalQueueHash,
        uint64 withdrawalBatchIndex
    );

    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    /// @notice Tempo gas rate (token units per gas unit on Tempo).
    /// @dev Fee = gasLimit * tempoGasRate. User must estimate total gas needed.
    function tempoGasRate() external view returns (uint128);

    /// @notice Next withdrawal index (monotonically increasing).
    function nextWithdrawalIndex() external view returns (uint64);

    /// @notice Current withdrawal batch index (monotonically increasing).
    function withdrawalBatchIndex() external view returns (uint64);

    /// @notice Last finalized batch parameters (for proof access via state root).
    function lastBatch() external view returns (LastBatch memory);

    /// @notice Number of pending withdrawals waiting to be batched.
    function pendingWithdrawalsCount() external view returns (uint256);

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    function transferSequencer(address newSequencer) external;

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external;

    /// @notice Set Tempo gas rate. Only callable by sequencer.
    function setTempoGasRate(uint128 _tempoGasRate) external;

    /// @notice Maximum number of withdrawal requests per zone block (0 = unlimited).
    function maxWithdrawalsPerBlock() external view returns (uint256);

    /// @notice Set maximum withdrawal requests per zone block. Only callable by sequencer.
    /// @dev Set to 0 for unlimited. Provides rate-limiting in addition to the gas fee.
    function setMaxWithdrawalsPerBlock(uint256 _maxWithdrawalsPerBlock) external;

    /// @notice Calculate the fee for a withdrawal with the given gasLimit.
    /// @dev Fee = gasLimit * tempoGasRate. User must estimate total gas needed.
    function calculateWithdrawalFee(uint64 gasLimit) external view returns (uint128);

    /// @notice Request a withdrawal from the zone back to Tempo.
    /// @dev Caller must have approved the outbox to spend `amount + fee` of the specified token.
    ///      The token must be enabled on the portal. Withdrawals can never be disabled
    ///      for an enabled token (non-custodial guarantee).
    ///      Subject to maxWithdrawalsPerBlock cap if set by the sequencer.
    ///      Tokens are burned immediately and withdrawal is stored in pending array.
    /// @param token The TIP-20 token to withdraw.
    /// @param to The Tempo recipient address.
    /// @param amount Amount to send to recipient (fee is additional).
    /// @param memo User-provided context (e.g., payment reference).
    /// @param gasLimit Gas limit for messenger callback (0 = no callback).
    /// @param fallbackRecipient Zone address for bounce-back if callback fails.
    /// @param data Calldata for the target (max 1KB).
    /// @param revealTo Compressed secp256k1 public key (33 bytes) to encrypt sender reveal to, or empty.
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

    /// @notice Finalize batch at end of final block - build withdrawal hash and write to state.
    /// @dev Only callable by sequencer in the final block of a batch.
    ///      Must be called exactly once per batch (count may be 0). Writes `withdrawalQueueHash`
    ///      and `withdrawalBatchIndex` to lastBatch storage for proof access.
    /// @param count Max number of withdrawals to process (limits per-call gas cost).
    /// @return withdrawalQueueHash The hash chain for Tempo batch submission.
    function finalizeWithdrawalBatch(uint256 count) external returns (bytes32 withdrawalQueueHash);
}
```

The `finalizeWithdrawalBatch()` function constructs the hash chain on-chain by processing withdrawals in reverse order (newest to oldest), so the oldest ends up outermost for O(1) Tempo removal:

```
// On-chain hash chain construction (inside finalizeWithdrawalBatch())
withdrawalQueueHash = EMPTY_SENTINEL
for i from (pendingCount - 1) down to 0:
    withdrawalQueueHash = keccak256(abi.encode(withdrawals[i], withdrawalQueueHash))
    pop withdrawal from storage
```

### External dependencies

#### TIP-20 (minimal)

```solidity
interface ITIP20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function approve(address spender, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}
```

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
- **Withdrawal queue**: fixed-size ring buffer with `WITHDRAWAL_QUEUE_CAPACITY = 100` (each batch gets its own slot, `head` points to oldest unprocessed batch, `tail` points to where next batch writes, slots are indexed via `head/tail % WITHDRAWAL_QUEUE_CAPACITY`)

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
- **Zone processing**: The zone's `advanceTempo()` processes deposits in FIFO order (oldest first, working outward from its `processedDepositQueueHash`), and validates the result matches `currentDepositQueueHash` (read from Tempo state).
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
- **Fixed-size ring buffer**: Each batch gets its own slot (capacity 100). Sequencer processes from `head`, proofs add at `tail`. Reverts with `WithdrawalQueueFull` if all slots are occupied.

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

The key insight: structure the hash chain so the **on-chain operation touches the outermost layer**. Additions wrap the outside; removals unwrap from the outside. The expensive operation (processing the full queue) happens inside the ZKP where O(N) is acceptable. Using `EMPTY_SENTINEL` (0xffffffff...fff) instead of 0x00 avoids storage clearing and gas refund incentive issues.

## Bridging in (Tempo to zone)

1. User calls `ZonePortal.deposit(token, to, amount, memo)` on Tempo, specifying which enabled TIP-20 to deposit.
2. `ZonePortal` validates the token is enabled and deposits are active, transfers `amount` of the specified token into escrow, and appends a deposit to the queue: `currentDepositQueueHash = keccak256(abi.encode(DepositType.Regular, deposit, currentDepositQueueHash))`. The `Deposit` struct includes the `token` field.
3. The sequencer observes `DepositMade` events and processes deposits in order via `ZoneInbox.advanceTempo()`, minting the correct zone-side TIP-20 to the recipient: `IZoneToken(d.token).mint(d.to, d.amount)`. Deposits always succeed—there is no callback or bounce mechanism.
4. A batch proof/attestation must prove the zone correctly processed deposits by validating the Tempo state read inside the proof.
5. After the batch is accepted, `lastSyncedTempoBlockNumber` is updated to record how far Tempo state was synced.

Notes:

- Deposits are simple token credits. There are no callbacks or failure modes on the zone side.
- Deposits are finalized for Tempo once the batch is verified.
- There is no forced inclusion. If the sequencer withholds deposits, funds are stuck in escrow.
- The portal only stores `currentDepositQueueHash`, not individual deposits. The sequencer must track deposits off-chain.
- Tempo state advancement is combined with deposit processing in `ZoneInbox.advanceTempo()`, which calls `TempoState.finalizeTempo()` internally.
- The proof validates an exact match to `currentDepositQueueHash` from Tempo state, ensuring it cannot claim to process fake deposits.
- Each enabled TIP-20 is deployed at the **same address** on the zone as on Tempo. The zone node must deploy/configure zone-side representations for each enabled token.

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
    address token;               // TIP-20 token (public, for escrow accounting)
    address sender;              // Depositor (public, for refunds)
    uint128 amount;              // Amount (public, for accounting)
    uint256 keyIndex;            // Index of encryption key used (specified by depositor)
    EncryptedDepositPayload encrypted; // Encrypted (to, memo)
}
```

**What's public vs. private:**

| Field | Visibility | Reason |
|-------|------------|--------|
| `token` | Public | Needed for on-chain escrow accounting and zone-side minting |
| `sender` | Public | Needed for potential refunds if decryption fails |
| `amount` | Public | Needed for on-chain accounting/escrow |
| `to` | Encrypted | Privacy - only sequencer knows recipient |
| `memo` | Encrypted | Privacy - only sequencer knows payment context |

**Processing flow:**

1. User calls `depositEncrypted(token, amount, keyIndex, encrypted)` on Tempo portal
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
    IZoneToken(ed.token).mint(ed.sender, ed.amount);
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

Usage in `ZoneInbox.advanceTempo()`:

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

// Step 3: Derive AES key from shared secret using HKDF-SHA256 (in Solidity)
bytes32 aesKey = _hkdfSha256(
    dec.sharedSecret,
    "ecies-aes-key",  // salt
    ""                // info (empty)
);

// Step 4: Decrypt using AES-256-GCM precompile
(bytes memory decryptedPlaintext, bool valid) = IAesGcmDecrypt(AES_GCM_DECRYPT).decrypt(
    aesKey,
    ed.encrypted.nonce,
    ed.encrypted.ciphertext,
    "",  // empty AAD
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
    // Decryption failed - return funds to sender
    IZoneToken(ed.token).mint(ed.sender, ed.amount);
    emit EncryptedDepositFailed(currentHash, ed.sender, ed.token, ed.amount);
} else {
    // Decryption succeeded - mint correct zone-side TIP-20 to decrypted recipient
    IZoneToken(ed.token).mint(dec.to, ed.amount);
    emit EncryptedDepositProcessed(currentHash, ed.sender, dec.to, ed.token, ed.amount, dec.memo);
}
```

**Key properties:**
- **Zero-knowledge security**: Chaum-Pedersen proof verifies shared secret without exposing sequencer's private key to EVM
- **Griefing resistance**: Invalid encryptions can be detected and rejected, preventing chain blockage
- **Graceful failure**: Invalid encrypted deposits return funds to sender instead of reverting
- **Cryptographic proof**: GCM tag validation proves decryption correctness
- **On-chain verification**: All verification happens on-chain via precompiles
- **Standard crypto**: Uses well-established ECIES, ECDH, Chaum-Pedersen, HKDF-SHA256, and AES-256-GCM

**Precompile implementation notes:**

*Chaum-Pedersen Verify (`0x1c00000000000000000000000000000000000100`):*
- Input: ephemeralPub, sharedSecret, sequencerPub, proof (s, c)
- Reconstruct commitments: `R1 = s*G - c*pubSeq`, `R2 = s*ephemeralPub - c*sharedSecretPoint`
- where `sharedSecretPoint` is derived by lifting sharedSecret to curve (requires Y-coordinate recovery)
- Recompute challenge: `c' = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)`
- Verify: `c == c'`
- Gas cost: ~8000 gas (2 point multiplications + 2 point additions + hash)

*AES-GCM Decrypt (`0x1c00000000000000000000000000000000000101`):*
- Input: AES key (32 bytes), nonce (12 bytes), ciphertext, AAD, tag (16 bytes)
- Decrypt: `plaintext = AES-256-GCM-Decrypt(key, nonce, ciphertext, aad, tag)`
- Return decrypted plaintext and `true` if tag validates, or empty bytes and `false` otherwise
- Gas cost: ~1000 gas base + ~500 per 32 bytes of ciphertext
- Much simpler than full ECIES precompile - only handles symmetric encryption

*HKDF-SHA256 (implemented in Solidity):*
- Uses existing SHA256 precompile (0x02) to implement HMAC-SHA256 and HKDF
- HMAC-SHA256: `HMAC(key, msg) = SHA256((key ⊕ opad) || SHA256((key ⊕ ipad) || msg))`
- HKDF-Extract: `PRK = HMAC-SHA256(salt, IKM)`
- HKDF-Expand: `OKM = HMAC-SHA256(PRK, info || 0x01)`
- Gas cost: ~5-10k gas for full HKDF (depends on message sizes, dominated by SHA256 calls)
- Tradeoff: Higher gas cost than native implementation, but keeps precompile minimal and auditable

**Encryption key history:**

To support key rotation, the portal stores all historical encryption keys. Users explicitly specify which key index they encrypted to, solving the race condition where a key might rotate between transaction signing and block inclusion.

```solidity
/// @notice Historical record of an encryption key with its activation block
struct EncryptionKeyEntry {
    bytes32 x;              // X coordinate of the public key
    uint8 yParity;          // Y coordinate parity (0x02 or 0x03)
    uint64 activationBlock; // Tempo block number when this key became active
}

/// @notice Encrypted deposit includes the key index and token used for encryption
struct EncryptedDeposit {
    address token;               // TIP-20 token (public, for escrow accounting)
    address sender;              // Depositor (public, for refunds)
    uint128 amount;              // Amount (public, for accounting)
    uint256 keyIndex;            // Index of encryption key used (specified by depositor)
    EncryptedDepositPayload encrypted; // Encrypted (to, memo)
}

/// @notice Deposit function requires explicit key index and token
function depositEncrypted(
    address token,                          // Token is public (needed for escrow)
    uint128 amount,
    uint256 keyIndex,                      // User specifies which key they encrypted to
    EncryptedDepositPayload calldata encrypted
) external returns (bytes32 newCurrentDepositQueueHash);
```

Key management functions:

- `setSequencerEncryptionKey(x, yParity, popV, popR, popS)` - Appends a new key to history, active from current Tempo block. Requires a proof of possession (ECDSA signature over `keccak256(abi.encode(portalAddress, x, yParity))` by the corresponding private key)
- `encryptionKeyCount()` - Returns total number of keys in history
- `encryptionKeyAt(index)` - Returns a historical key entry by index
- `encryptionKeyAtBlock(tempoBlockNumber)` - Returns the key that was active at a specific block
- `isEncryptionKeyValid(keyIndex)` - Check if a key can still be used for new deposits

**Why keyIndex instead of block number:**

The user specifies `keyIndex` at signing time (when they know which key they're encrypting to). This avoids race conditions:
- If key rotates *after* signing but *before* inclusion, the deposit still references the correct key
- The portal can validate that `keyIndex` is a valid historical key
- The prover looks up `encryptionKeyAt(keyIndex)` to get the decryption key

**Key expiration:**

Old encryption keys expire after a grace period to limit how long the sequencer must retain old private keys:

```solidity
/// 1 day at 1 second block time = 86400 blocks
uint64 constant ENCRYPTION_KEY_GRACE_PERIOD = 86400;

error EncryptionKeyExpired(uint256 keyIndex, uint64 activationBlock, uint64 supersededAtBlock);
```

- When a new key is set, the previous key remains valid for `ENCRYPTION_KEY_GRACE_PERIOD` blocks
- After that, deposits using the old key are rejected with `EncryptionKeyExpired`
- Users should call `isEncryptionKeyValid(keyIndex)` before signing to check if their key is still valid
- The current (latest) key never expires

Example timeline:
1. Block 1000: Key 0 is set (current)
2. Block 5000: Key 1 is set (current), Key 0 expires at block 5000 + 86400 = 91400
3. Block 91400: Deposits with Key 0 start being rejected
4. Sequencer can delete Key 0's private key after block 91400

This allows:
1. Users to verify they're encrypting to the current key before signing
2. The prover to determine which key to use for any deposit via explicit `keyIndex`
3. Seamless key rotation with a grace period for in-flight transactions
4. Sequencer to safely delete old private keys after expiration

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

    uint256 slotIndex = head % WITHDRAWAL_QUEUE_CAPACITY;
    bytes32 currentSlot = _withdrawalQueue.slots[slotIndex];

    // Verify head (remainingQueue of 0 means last item, we check against EMPTY_SENTINEL)
    bytes32 expectedRemainingQueue = remainingQueue == bytes32(0) ? EMPTY_SENTINEL : remainingQueue;
    require(keccak256(abi.encode(w, expectedRemainingQueue)) == currentSlot, "invalid");

    // Pop the withdrawal regardless of success/failure
    if (remainingQueue == bytes32(0)) {
        // Slot exhausted, mark as empty and advance head
        _withdrawalQueue.slots[slotIndex] = EMPTY_SENTINEL;
        _withdrawalQueue.head = head + 1;
    } else {
        // More withdrawals in this slot
        _withdrawalQueue.slots[slotIndex] = remainingQueue;
    }

    if (w.gasLimit == 0) {
        ITIP20(w.token).transfer(w.to, w.amount);
        return;
    }

    // Try callback via messenger for atomicity
    try IZoneMessenger(messenger).relayMessage(w.token, w.senderTag, w.to, w.amount, w.gasLimit, w.callbackData) {
        // Success: tokens transferred and callback executed
    } catch {
        // Callback failed: bounce back to zone
        _enqueueBounceBack(w.token, w.amount, w.fallbackRecipient);
    }
}
```

The messenger does `ITIP20(token).transferFrom(portal, target, amount)` then executes the callback. Both are atomic: if the callback reverts, the transferFrom reverts too, and funds remain in the portal for bounce-back. Receivers check `msg.sender == messenger` to authenticate the call, and receive the `senderTag` in `onWithdrawalReceived` (see [Authenticated withdrawals](#authenticated-withdrawals)). This enables composable withdrawals where funds can flow directly into Tempo contracts (e.g., DEX swaps, staking, cross-zone deposits).

## Withdrawal failure and bounce-back

Withdrawals can fail if the token transfer or messenger callback reverts (out of gas, logic error, TIP-403 policy, token pause, etc.). When this happens, the portal "bounces back" the funds by re-depositing into the same zone to the withdrawal's `fallbackRecipient`.

```solidity
function _enqueueBounceBack(address token, uint128 amount, address fallbackRecipient) internal {
    Deposit memory d = Deposit({
        token: token,           // same token from the failed withdrawal
        sender: address(this),
        to: fallbackRecipient,
        amount: amount,
        memo: bytes32(0)
    });
    currentDepositQueueHash = keccak256(abi.encode(DepositType.Regular, d, currentDepositQueueHash));
    emit BounceBack(...);
}
```

The zone processes bounce-back deposits and credits the `fallbackRecipient`. This allows withdrawals to fail gracefully without blocking the queue.

## Data availability and liveness

- Zone data availability is fully trusted to the sequencer.
- If the sequencer withholds data or halts, users cannot reconstruct zone state or force exits; batch posting and withdrawal processing are sequencer-only.
- The design assumes users accept this risk in exchange for low-cost and fast settlement.

## Withdrawal failure details

Withdrawals can fail for various reasons. The system handles failures gracefully via bounce-back:

### Failure reasons

Withdrawals can fail due to:
- **Transfer failure**: `transfer` or `transferFrom` reverts (includes gasLimit = 0 cases)
- **TIP-403 policy**: Recipient not authorized under the token's transfer policy
- **Token paused**: The token is globally paused
- **Callback revert**: The receiver contract reverts (out of gas, logic error, etc.)
- **Callback rejection**: Receiver returns wrong selector

### Withdrawal failures (Tempo-side)

When a withdrawal fails on Tempo:
1. The TIP-20 transfer or callback reverts
2. The portal enqueues a bounce-back deposit to `fallbackRecipient` on the zone
3. Funds return to zone via the normal deposit flow

### TIP-403 specific considerations

Tempo TIP-20 tokens use TIP-403 for transfer authorization:
- Every transfer checks `isAuthorized(policyId, from)` AND `isAuthorized(policyId, to)`
- Policy types: WHITELIST (must be in set) or BLACKLIST (must not be in set)
- Policy ID 1 is "always-allow" (default for most tokens)

Zone creators SHOULD choose tokens with `transferPolicyId == 1` to avoid complexity. If using restricted policies:
- The portal address MUST be whitelisted
- Users should set `fallbackRecipient` to an address they control

## Security considerations

- Sequencer can halt the zone without recourse due to missing data availability.
- The verifier is a trust anchor. A faulty verifier can steal or lock funds.
- Withdrawals with callbacks go through the zone messenger with a user-specified gas limit. The messenger does `transferFrom` + callback atomically; any transfer or callback failure triggers a bounce-back to `fallbackRecipient`.
- Deposits are locked on Tempo until a verified batch consumes them.
- **Bounce-back guarantees**: Failed withdrawals bounce back to zone `fallbackRecipient`. Users always retain their funds.
- **TIP-403 policy changes**: If a token's policy restricts the portal, withdrawals for that token will fail and bounce back.
- **Token pause**: If a token is paused, withdrawals for that token bounce back to zone.

## Implementation architecture

This section describes the concrete implementation approach for zone nodes.

### Node architecture

Each zone runs as separate Tempo zone node (based on the Tempo client). It uses chain notifications to subscribe to all finalized updates on contracts that are read from on L1.

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
├── ExEx: Zone A (multi-asset: USDC, USDT, DAI, ...)
│   ├── TIP-20 Precompiles (per enabled token)
│   ├── Payments Precompile
│   ├── In-memory State Store
│   └── SP1 Prover (mock for dev)
│
└── ExEx: Zone B (multi-asset: USDC, EURC, ...)
    ├── TIP-20 Precompiles (per enabled token)
    ├── Payments Precompile
    ├── In-memory State Store
    └── SP1 Prover (mock for dev)
```

## Authenticated withdrawals

Zone transactions are private — the zone is a validium and transaction data is not published on L1. However, when a withdrawal is processed on L1 via `processWithdrawal`, the full `Withdrawal` struct (including `sender`) is passed in calldata and is publicly visible. This leaks the sender's identity.

Authenticated withdrawals replace the plaintext sender with a commitment that only the sender can open, enabling selective disclosure: the recipient (or any other party the sender chooses) can verify the sender's identity, while the public cannot.

### Sender tag

The `sender` field in the `Withdrawal` struct is replaced with a `senderTag` and a new `encryptedSender` field:

```solidity
struct Withdrawal {
    address token;
    bytes32 senderTag;          // keccak256(abi.encodePacked(sender, txHash))
    address to;
    uint128 amount;
    uint128 fee;
    bytes32 memo;
    uint64 gasLimit;
    address fallbackRecipient;
    bytes callbackData;
    bytes encryptedSender;      // ECDH-encrypted (sender, txHash) for a target key, or empty
}
```

The sequencer computes both fields when building the withdrawal in `finalizeWithdrawalBatch`:

```
senderTag       = keccak256(abi.encodePacked(sender, txHash))
encryptedSender = ECDH_Encrypt((sender, txHash), revealTo)   // empty if no revealTo
```

where `sender` is the address that called `requestWithdrawal` on the zone and `txHash` is the hash of that transaction. The `txHash` acts as a 32-byte blinding factor — it is private to the zone (transaction data is not published on L1) and known only to the sender and the sequencer.

The sender cannot encrypt `txHash` into the withdrawal transaction themselves (the hash depends on the transaction contents, creating a circular dependency). Instead, the sequencer performs the encryption post-hoc: it knows `sender`, `txHash`, and the target key after processing the transaction.

### Reveal key

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

If `revealTo` is provided, the sequencer encrypts `(sender, txHash)` to that key using ECDH (same scheme as encrypted deposits) and populates `encryptedSender` in the L1-facing `Withdrawal` struct. If empty, `encryptedSender` is empty and the sender can reveal `txHash` manually.

The `revealTo` key is stored in the zone's pending withdrawal state so the sequencer can use it during `finalizeWithdrawalBatch`. It does not appear in the L1-facing struct — only the encrypted output does.

### Encrypted sender format

When `revealTo` is specified, `encryptedSender` contains:

```
ephemeralPubKey (33 bytes) || ciphertext (52 bytes) || mac (16 bytes)
```

The sequencer generates an ephemeral key pair `(r, R = r*G)`, derives a shared secret `S = r * revealTo`, and encrypts `abi.encodePacked(sender, txHash)` (52 bytes) using a symmetric cipher keyed by `S`. The holder of the private key corresponding to `revealTo` derives the same shared secret via `S = privKey * R` and decrypts.

### Selective disclosure

Two disclosure modes:

**Manual reveal** (`revealTo` empty): The sender reveals `txHash` to any party off-chain. The verifier checks `keccak256(abi.encodePacked(sender_address, txHash)) == senderTag`.

**Encrypted reveal** (`revealTo` specified): The holder of the `revealTo` private key decrypts `encryptedSender` to obtain `(sender, txHash)` and verifies against `senderTag`. No off-chain communication with the sender is needed.

The sender can use both modes: specify `revealTo` for a primary recipient (e.g., target zone sequencer) and later reveal `txHash` manually to additional parties.

### Zone-to-zone transfers

For cross-zone withdrawals (Zone A to Zone B), the sender sets `revealTo` to Zone B's sequencer public key. The flow:

1. Sender on Zone A calls `requestWithdrawal` with `revealTo = pubKeySeqB`.
2. Zone A's sequencer processes the transaction, computes `senderTag` and `encryptedSender`.
3. The withdrawal is proven and submitted to L1. `processWithdrawal` transfers tokens to Zone B's portal.
4. Zone B's sequencer observes the incoming deposit, reads `encryptedSender` from the withdrawal data.
5. Zone B's sequencer decrypts with its private key to learn `(sender, txHash)`.
6. Zone B's sequencer verifies `keccak256(sender || txHash) == senderTag`.
7. Zone B can now attribute the deposit to the sender, enabling sender-aware processing on Zone B.

Each zone sequencer's public key is already published (used for encrypted deposits), so the sender can look it up without additional infrastructure.

### Trust model

The sequencer computes `senderTag` and `encryptedSender`, and includes them in the `Withdrawal` struct. The struct is hashed into the withdrawal queue chain, which is committed in the batch proof. The sequencer is trusted to compute both correctly — a malicious sequencer could insert incorrect values, and the batch proof would still be valid since the prover does not verify the tag's preimage or the encryption.

This is a modest extension of the existing trust model: the sequencer is already trusted for liveness, transaction ordering, and withdrawal processing. Adding honest sender tag computation and encryption to that set is a small incremental assumption.

To upgrade to trustless sender authentication, the `senderTag` computation can be moved into the ZK circuit. The prover would verify `senderTag == keccak256(abi.encodePacked(sender, txHash))` for the actual sender of each withdrawal transaction. The encryption would remain sequencer-mediated (ZK-proving ECDH encryption is expensive), but the commitment would be trustless. The on-chain interface and reveal flow remain unchanged.

### Impact on callback withdrawals

For simple withdrawals (`gasLimit == 0`), the sender field is not used in execution — only `to` and `amount` matter. The `senderTag` replacement has no functional impact.

For callback withdrawals (`gasLimit > 0`), `IWithdrawalReceiver.onWithdrawalReceived` receives `bytes32 senderTag` instead of `address sender`:

```solidity
function onWithdrawalReceived(
    bytes32 senderTag,
    address token,
    uint128 amount,
    bytes calldata callbackData
) external returns (bytes4);
```

Receiving contracts that need the sender's identity can decrypt `encryptedSender` off-chain (if they hold the `revealTo` key) or receive `txHash` via `callbackData` or a separate channel.

### Zone-side changes

`ZoneOutbox.requestWithdrawal` records the pending withdrawal with the plaintext `sender` and the `revealTo` key. The sequencer computes `senderTag` and `encryptedSender` when building the L1-facing `Withdrawal` struct in `finalizeWithdrawalBatch`. The zone-side `WithdrawalRequested` event continues to include the plaintext `sender` since zone events are private.

### Open questions

- **Sequencer-signed tag**: A per-withdrawal sequencer signature over `senderTag` would let recipients verify authenticity without trusting the batch proof context. This adds ~65 bytes per withdrawal. Whether this overhead is justified depends on the verification use case.
- **revealTo for non-zone recipients**: Individual L1 recipients could also publish a public key (e.g., via ENS or an on-chain registry) to receive encrypted sender reveals without manual coordination.

## Open questions

- Should deposits be cancellable if not consumed within a timeout?
