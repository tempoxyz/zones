# Tempo Zones (Draft)

This document proposes a new validium protocol designed for Tempo. It is a design overview, not a full specification.

## Goals

- Create a Tempo-native validium called a zone.
- Each zone has exactly one permissioned sequencer.
- Each zone bridges exactly one TIP-20 token, which is also the zone gas token.
- Settlement uses fast validity proofs or TEE attestations (ZK or TEE). Data availability is fully trusted to the sequencer.
- Cross-chain operations are Tempo-centric: bridge in (simple deposit), bridge out (with optional callback to receiver contracts for L1 composability).
- Verifier is abstracted behind a minimal `IVerifier` interface.
- Liveness (including exits) is wholly dependent on the permissioned sequencer; there is no permissionless fallback.

## Non-goals

- No attempt to solve data availability, forced inclusion, or censorship resistance.
- No upgradeability or governance model.
- No general messaging or multi-asset bridging. Only one TIP-20 per zone.

## Terminology

- Tempo: the L1 chain.
- Zone: the validium chain anchored to Tempo.
- Gas token: the zone's only TIP-20, bridged from Tempo.
- Portal: the Tempo-side contract that escrows the gas token and finalizes exits.
- Batch: a sequencer-produced commitment covering one or more zone blocks, ending with a `finalizeBatch()` call. The sequencer controls batch frequency.

## System overview

### Actors

- Zone sequencer: permissioned operator that orders zone transactions, provides data, and posts batches to Tempo. The sequencer is the only actor that submits transactions to the portal.
- Verifier: ZK proof system or TEE attester. Abstracted via `IVerifier`.
- Users: deposit TIP-20 from Tempo to the zone or exit back to Tempo.

### Tempo contracts

- `ZoneFactory`: creates zones and registers parameters.
- `ZonePortal`: per-zone portal that escrows the gas token on Tempo and finalizes exits.

### Zone components (off-chain or zone-side)

- `ZoneSequencer`: collects transactions and creates batches.
- `ZoneExecutor`: executes the zone state transition.
- `ZoneProver` or `ZoneAttester`: produces proof/attestation for each batch.

## Zone creation

A zone is created via `ZoneFactory.createZone(...)` with:

- `token`: the Tempo TIP-20 address to bridge. This is the only bridged token and the gas token.
- `sequencer`: permissioned sequencer address.
- `verifier`: `IVerifier` implementation for proof or attestation.
- `zoneParams`: initial configuration (genesis block hash, fee parameters).

The factory deploys a `ZonePortal` that escrows the gas token on Tempo. The zone genesis includes the portal address and the gas token configuration.

## Execution and fees

- The zone reuses Tempo's fee units and accounting model.
- The fee token is always the gas token. There is no fee token selection.
- Transactions use Tempo transaction semantics for fee payer, max fee per gas, and gas limit. The fee token field is fixed to the gas token.

## Batch submission

The sequencer posts batches to Tempo via a single `submitBatch` call (sequencer-only) that:

1. Verifies the proof/attestation for the state transition (including batch chain integrity via `prevBatchBlockNumber`).
2. Updates the portal's `blockHash`, `blockNumber`, and `lastSyncedTempoBlockNumber`.
3. Updates the withdrawal queue (adds new withdrawals to the next slot in the unbounded buffer).

Each batch submission includes:

- `tempoBlockNumber` (the Tempo block number for blockhash verification)
- `nextBlockHash` (the zone block hash after execution)
- `batchBlockNumber` (the zone block number of this batch, from the `BatchFinalized` event)
- `withdrawalQueueHash` (hash chain of withdrawals for this batch, or 0 if none)
- `verifierConfig` (opaque payload forwarded to the verifier for domain separation/attestation needs)
- `proof` (validity proof or TEE attestation)

The portal tracks `blockHash` and `blockNumber` (the last proven batch block), `lastSyncedTempoBlockNumber` (the Tempo block the zone has synced to), `currentDepositQueueHash` (head of deposit queue), and an unbounded buffer for withdrawals with `head`, `tail`, and `maxSize` indices.

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
///      that nextProcessedHash is a valid ancestor. No ceiling needed on-chain.
struct DepositQueueTransition {
    bytes32 prevProcessedHash;     // where proof starts (verified against zone state)
    bytes32 nextProcessedHash;     // where zone processed up to (proof output)
}

/// @notice Withdrawal queue transition for batch proofs
struct WithdrawalQueueTransition {
    bytes32 withdrawalQueueHash;  // hash chain of withdrawals for this batch (0 if none)
}

interface IVerifier {
    /// @notice Verify a batch proof
    /// @dev The proof validates:
    ///      1. Valid state transition from prevBlockHash to nextBlockHash
    ///      2. Zone's TempoState.tempoBlockHash() matches tempoBlockHash
    ///      3. BatchFinalized event exists in final block with correct batchIndex
    ///      4. Deposit processing is correct (validated via Tempo state read inside proof)
    function verify(
        bytes32 tempoBlockHash,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
    ) external view returns (bool);
}
```

The verifier validates that:
1. The state transition from `prevBlockHash` to `nextBlockHash` is correct (all transactions executed properly).
2. The zone's `TempoState.tempoBlockHash()` matches the `tempoBlockHash` provided by the portal.
3. A `BatchFinalized` event exists in the final block with the correct `batchIndex`.
4. Deposit processing is correct: the zone read `currentDepositQueueHash` from Tempo state and processed deposits accordingly.

The zone has access to Tempo state via the TempoState predeploy, so the proof can read `currentDepositQueueHash` directly from Tempo storage at the proven block. This eliminates the need for an on-chain "ceiling" slot.

`verifierData` + `proof` are opaque to the portal: ZK systems can ignore `verifierData`, while TEEs can pack attestation envelopes/quotes and measurement checks into `verifierData` for the verifier contract to enforce.

`submitBatch` verifies that `prevBlockHash == blockHash`, then calls the verifier. On success it updates `batchIndex`, `blockHash`, `lastSyncedTempoBlockNumber`, adds withdrawals to the queue, and emits `BatchSubmitted` with every verifier input/output (except the proof bytes) so off-chain observers can audit the batch.

### Deposit queue

Tempo to zone communication uses a single `depositQueue` chain. Each deposit is hashed into a chain:

```
newHash = keccak256(abi.encode(deposit, prevHash))
```

Where `deposit` is a `Deposit` struct containing the sender, recipient, amount, and memo. Tempo state advancement and deposit processing are combined in the ZoneInbox's `advanceTempo()` function, which calls `TempoState.finalizeTempo()` internally.

The portal tracks `currentDepositQueueHash` where new deposits land. The zone tracks its own `processedDepositQueueHash` in EVM state.

**Proof requirements**: The proof validates deposit processing by reading `currentDepositQueueHash` from Tempo state inside the proof. The zone's `advanceTempo()` function processes deposits and updates the zone's `processedDepositQueueHash`. The proof ensures this was done correctly by validating the Tempo state read.

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

To process the oldest withdrawal, the sequencer provides the withdrawal data and the remaining queue hash. The portal verifies the hash and updates the slot:

```solidity
function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external onlySequencer {
    uint256 head = _withdrawalQueue.head;

    // Check if queue is empty
    if (head == _withdrawalQueue.tail) {
        revert NoWithdrawalsInQueue();
    }

    bytes32 currentSlot = _withdrawalQueue.slots[head];

    // Verify this is the head of the current slot
    // The remainingQueue for the last item should be 0 (we convert to EMPTY_SENTINEL internally)
    bytes32 expectedRemainingQueue = remainingQueue == bytes32(0) ? EMPTY_SENTINEL : remainingQueue;
    require(keccak256(abi.encode(w, expectedRemainingQueue)) == currentSlot, "invalid");

    _executeWithdrawal(w);

    if (remainingQueue == bytes32(0)) {
        // Slot exhausted, mark as empty and advance head
        _withdrawalQueue.slots[head] = EMPTY_SENTINEL;
        _withdrawalQueue.head = head + 1;
    } else {
        // More withdrawals in this slot
        _withdrawalQueue.slots[head] = remainingQueue;
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

## Interfaces and functions

This section defines the functions and interfaces used by the design. The signatures are Solidity-style and focus on the minimum surface area.

### Common types

```solidity

struct ZoneInfo {
    uint64 zoneId;
    address portal;
    address messenger;
    address token;
    address sequencer;
    address verifier;
    bytes32 genesisBlockHash;
    bytes32 genesisTempoBlockHash;
    uint64 genesisTempoBlockNumber;
}

struct Deposit {
    address sender;
    address to;
    uint128 amount;
    bytes32 memo;
}

struct Withdrawal {
    address sender;             // who initiated the withdrawal on the zone
    address to;                 // Tempo recipient
    uint128 amount;
    bytes32 memo;
    uint64 gasLimit;            // max gas for IWithdrawalReceiver callback (0 = no callback)
    address fallbackRecipient;  // zone address for bounce-back if call fails
    bytes callbackData;         // calldata for IWithdrawalReceiver (if gasLimit > 0)
}
```

### Verifier

```solidity
interface IVerifier {
    function verify(
        bytes32 tempoBlockHash,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
    ) external view returns (bool);
}
```

The verifier receives the `tempoBlockHash` (looked up on-chain via `blockhash(tempoBlockNumber)`), block transition, deposit queue transition, withdrawal queue transition, and the proof. The proof must demonstrate that the zone's internal Tempo view matches `tempoBlockHash`, that the state transition is valid, and that the `lastBatch` storage in ZoneOutbox contains the correct `batchIndex` and batch parameters.

### Queue libraries

The portal uses two queue libraries that encapsulate the hash chain management patterns:

#### DepositQueueLib

Handles L1→L2 deposits. The L1 portal only tracks `currentDepositQueueHash` (the head of the queue where new deposits land). The zone tracks its own `processedDepositQueueHash` in EVM state, and the proof validates deposit processing by reading `currentDepositQueueHash` from Tempo state.

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

#### WithdrawalQueueLib (unbounded buffer)

Handles L2→L1 withdrawals where the producer (proof) is slow and the consumer (on-chain) is fast. Each batch gets its own slot in an unbounded buffer.

```solidity
/// @dev Sentinel value for empty slots. Using 0xff...ff instead of 0x00 to avoid
///      clearing storage (which would refund gas and create incentive issues).
bytes32 constant EMPTY_SENTINEL = bytes32(type(uint256).max);

struct WithdrawalQueue {
    uint256 head;     // slot index of oldest unprocessed batch
    uint256 tail;     // slot index where next batch will write
    uint256 maxSize;  // maximum queue length ever reached (for gas accounting)
    mapping(uint256 => bytes32) slots;  // hash chains per batch (EMPTY_SENTINEL = empty)
}

library WithdrawalQueueLib {
    /// @notice Add a batch's withdrawals to the queue (called during batch submission)
    /// @dev Writes to slot at tail, then advances tail. Updates maxSize if needed.
    function enqueue(WithdrawalQueue storage q, bytes32 withdrawalQueueHash) internal;

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

**Gas note**: Storage gas should only be charged when `(tail - head) > maxSize`. This is enforced by the precompile implementation.

| Queue | L1 operation | Zone/Proof operation |
|-------|--------------|---------------------|
| Deposit | `enqueue` (users deposit) | Process via `advanceTempo()` |
| Withdrawal | `dequeue` (sequencer processes) | Create via `finalizeBatch()` |

### Tempo contracts

#### Zone factory

```solidity
interface IZoneFactory {
    struct CreateZoneParams {
        address token;
        address sequencer;
        address verifier;
        bytes32 genesisBlockHash;
        bytes32 genesisTempoBlockHash;
        uint64 genesisTempoBlockNumber;
    }

    event ZoneCreated(
        uint64 indexed zoneId,
        address indexed portal,
        address indexed messenger,
        address token,
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
        address to,
        uint128 amount,
        bytes32 memo
    );

    event BatchSubmitted(
        uint64 indexed batchIndex,
        uint64 tempoBlockNumber,
        bytes32 nextProcessedDepositQueueHash,
        bytes32 nextBlockHash,
        uint256 withdrawalQueueTail,
        bytes32 withdrawalQueueHash
    );

    function zoneId() external view returns (uint64);
    function token() external view returns (address);
    function messenger() external view returns (address);
    function sequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
    function verifier() external view returns (address);
    function batchIndex() external view returns (uint64);
    function blockHash() external view returns (bytes32);
    function currentDepositQueueHash() external view returns (bytes32);
    function lastSyncedTempoBlockNumber() external view returns (uint64);
    function withdrawalQueueHead() external view returns (uint256);
    function withdrawalQueueTail() external view returns (uint256);
    function withdrawalQueueMaxSize() external view returns (uint256);
    function withdrawalQueueSlot(uint256 slot) external view returns (bytes32);

    /// @notice Set the sequencer's public key. Only callable by the sequencer.
    function setSequencerPubkey(bytes32 pubkey) external;

    /// @notice Deposit gas token into the zone. Returns the new current deposit queue hash.
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositQueueHash);

    /// @notice Process the next withdrawal from the queue. Only callable by the sequencer.
    /// @param withdrawal The withdrawal to process (must be at the head of the current slot).
    /// @param remainingQueue The hash of the remaining withdrawals in this slot (0 if last).
    function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external;

    /// @notice Submit a batch and verify the proof. Only callable by the sequencer.
    /// @dev Verifies prevBlockHash == blockHash, then calls the verifier.
    ///      On success updates batchIndex, blockHash, lastSyncedTempoBlockNumber,
    ///      and adds withdrawals to the queue.
    function submitBatch(
        uint64 tempoBlockNumber,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
    ) external;
}
```

#### Zone messenger (L1)

Each zone has a dedicated messenger contract on L1. The portal gives the messenger max approval for the gas token. Withdrawal callbacks originate from this contract, not the portal.

```solidity
interface IZoneMessenger {
    /// @notice Returns the zone's portal address
    function portal() external view returns (address);

    /// @notice Returns the gas token address
    function token() external view returns (address);

    /// @notice Returns the L2 sender during callback execution
    /// @dev Reverts if not in a callback context
    function xDomainMessageSender() external view returns (address);

    /// @notice Relay a withdrawal message. Only callable by the portal.
    /// @dev Transfers tokens from portal to target via transferFrom, then executes callback.
    ///      If callback reverts, the entire call reverts (including the transfer).
    /// @param sender The L2 origin address
    /// @param target The L1 recipient
    /// @param amount Tokens to transfer from portal to target
    /// @param gasLimit Max gas for the callback
    /// @param data Calldata for the target
    function relayMessage(
        address sender,
        address target,
        uint256 amount,
        uint256 gasLimit,
        bytes calldata data
    ) external;
}
```

The messenger does `token.transferFrom(portal, target, amount)` then calls the target with `data`. Both are atomic: if the callback reverts, the transfer is also reverted. Receivers check `msg.sender == zoneMessenger` and call `zoneMessenger.xDomainMessageSender()` to authenticate the L2 origin.

### Zone predeploys

#### Zone gas token

The zone's gas token is the bridged TIP-20 from Tempo. It is deployed at the **same address** on the zone as on Tempo. Users interact with it via the standard TIP-20 interface for transfers and approvals. The zone sequencer mints tokens when processing deposits and burns them when withdrawals are requested.

#### TempoState predeploy

The TempoState predeploy allows zones to verify they have a correct view of Tempo (L1) state. It stores the latest finalized Tempo block info and provides storage reading functionality.

```solidity
// Predeploy address: 0x1c00000000000000000000000000000000000000
interface ITempoState {
    /// @notice Current finalized Tempo block hash
    function tempoBlockHash() external view returns (bytes32);

    /// @notice Current finalized Tempo block number
    function tempoBlockNumber() external view returns (uint64);

    /// @notice Current finalized Tempo block timestamp (seconds)
    function tempoTimestamp() external view returns (uint64);

    /// @notice Current finalized Tempo state root (for storage proofs)
    function tempoStateRoot() external view returns (bytes32);

    /// @notice Current finalized Tempo receipts root (for event/log proofs)
    function tempoReceiptsRoot() external view returns (bytes32);

    /// @notice Finalize a Tempo block header. Only callable by sequencer.
    /// @dev Validates chain continuity (parent hash must match, number must be +1)
    /// @param header RLP-encoded Tempo header
    function finalizeTempo(bytes calldata header) external;

    /// @notice Read a storage slot from a Tempo contract
    /// @param account The Tempo contract address
    /// @param slot The storage slot to read
    /// @return value The storage value
    function readTempoStorageSlot(address account, bytes32 slot) external view returns (bytes32);

    /// @notice Read multiple storage slots from a Tempo contract
    /// @param account The Tempo contract address
    /// @param slots The storage slots to read
    /// @return values The storage values
    function readTempoStorageSlots(address account, bytes32[] calldata slots) external view returns (bytes32[] memory);
}
```

**How it works:**

1. The sequencer submits Tempo block headers via `finalizeTempo()`, which validates chain continuity and updates the stored state.
2. When submitting a batch, the prover specifies a `tempoBlockNumber`. The portal calls `blockhash(tempoBlockNumber)` to get the actual hash.
3. The proof must demonstrate that the zone's `tempoBlockHash` (from TempoState) matches the value passed by the portal.
4. The `readTempoStorageSlot` functions are precompile stubs - actual implementation is in the zone node, validated against `tempoStateRoot`.

Tempo state staleness depends on how frequently the sequencer calls `finalizeTempo()`. The prover includes Merkle proofs for each unique account and storage slot accessed during the batch.

#### TIP-403 registry

The zone has a `TIP403Registry` contract deployed at the **same address** as L1. This contract is read-only—it does not support writing policies. Its `isAuthorized` function reads policy state from L1 via the L1 state reader precompile, so zone-side TIP-20 transfers enforce L1 TIP-403 policies automatically.

#### Zone inbox

The zone inbox is a system contract that advances Tempo state and processes deposits from Tempo in a single atomic operation. It is called by the sequencer as a **system transaction at the start of each block**.

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
        uint128 amount,
        bytes32 memo
    );

    /// @notice The zone's last processed deposit queue hash.
    function processedDepositQueueHash() external view returns (bytes32);

    /// @notice Advance Tempo state and process deposits in a single system transaction.
    /// @dev This is the main entry point for the sequencer's system transaction.
    ///      1. Advances the zone's view of Tempo by processing the header
    ///      2. Processes deposits from the deposit queue
    ///      3. Validates the resulting hash against Tempo's currentDepositQueueHash
    /// @param header RLP-encoded Tempo block header
    /// @param deposits Array of deposits to process (oldest first, must be contiguous from processedDepositQueueHash)
    function advanceTempo(bytes calldata header, Deposit[] calldata deposits) external;
}
```

The sequencer observes `DepositMade` events on the Tempo portal and relays them to the zone via `advanceTempo`. This function:

1. Calls `TempoState.finalizeTempo(header)` to advance the zone's view of Tempo
2. Processes deposits in order, building the hash chain and minting gas tokens
3. Reads `currentDepositQueueHash` from the Tempo portal's storage via `TempoState.readTempoStorageSlot()`
4. Validates the resulting hash matches Tempo's current state

This combined approach ensures Tempo state advancement and deposit processing are atomic, and the deposit hash is validated against the actual Tempo state at the newly finalized block.

#### Zone outbox

The zone outbox handles withdrawal requests. Users approve the outbox to spend their gas tokens, then call `requestWithdrawal`. The outbox stores pending withdrawals in an array. When the sequencer is ready to finalize a block that will be batched, it calls `finalizeBatch(count)` as a system transaction at the end of the block. This constructs the withdrawal queue hash on-chain and writes batch parameters to storage. The event is emitted for observability, but the proof reads from state (via the `lastBatch` storage) rather than parsing event logs.

```solidity
/// @notice Batch parameters stored in state for proof access
struct LastBatch {
    bytes32 withdrawalQueueHash;
    uint64 tempoBlockNumber;
    bytes32 tempoBlockHash;
    uint64 batchIndex;
    uint64 batchBlockNumber;
}

interface IZoneOutbox {
    event WithdrawalRequested(
        uint64 indexed withdrawalIndex,
        address indexed sender,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes data
    );

    /// @notice Emitted when sequencer finalizes a batch at end of block.
    /// @dev Kept for observability. Proof reads from lastBatch storage instead.
    event BatchFinalized(
        bytes32 indexed withdrawalQueueHash,
        uint64 tempoBlockNumber,
        bytes32 tempoBlockHash,
        uint64 batchIndex,
        uint64 batchBlockNumber
    );

    /// @notice The gas token address (same as L1 portal's token).
    function gasToken() external view returns (address);

    /// @notice Next withdrawal index (monotonically increasing).
    function nextWithdrawalIndex() external view returns (uint64);

    /// @notice Current batch index (monotonically increasing).
    function batchIndex() external view returns (uint64);

    /// @notice Last finalized batch parameters (for proof access via state root).
    function lastBatch() external view returns (LastBatch memory);

    /// @notice Number of pending withdrawals waiting to be batched.
    function pendingWithdrawalsCount() external view returns (uint256);

    /// @notice Request a withdrawal from the zone back to Tempo.
    /// @dev Caller must have approved the outbox to spend `amount` of gas tokens.
    ///      Tokens are burned immediately and withdrawal is stored in pending array.
    /// @param to The Tempo recipient address.
    /// @param amount Amount to withdraw.
    /// @param memo User-provided context (e.g., payment reference).
    /// @param gasLimit Gas limit for messenger callback (0 = no callback).
    /// @param fallbackRecipient Zone address for bounce-back if callback fails.
    /// @param data Calldata for the target.
    function requestWithdrawal(
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes calldata data
    ) external;

    /// @notice Finalize batch at end of block - build withdrawal hash and write to state.
    /// @dev Only callable by sequencer as a system transaction at end of block.
    ///      Writes batch parameters to lastBatch storage for proof access.
    /// @param count Max number of withdrawals to process (avoids unbounded loops).
    /// @return withdrawalQueueHash The hash chain for L1 batch submission.
    function finalizeBatch(uint256 count) external returns (bytes32 withdrawalQueueHash);
}
```

The `finalizeBatch()` function constructs the hash chain on-chain by processing withdrawals in reverse order (newest to oldest), so the oldest ends up outermost for O(1) L1 removal:

```
// On-chain hash chain construction (inside finalizeBatch())
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

- **Deposit queue**: L1 tracks only `currentDepositQueueHash` (where new deposits land). The zone tracks its own `processedDepositQueueHash` in EVM state. The proof validates deposit processing by reading `currentDepositQueueHash` from Tempo state inside the proof.
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
- **Zone processing**: The zone's `advanceTempo()` processes deposits in FIFO order (oldest first, working outward from its `processedDepositQueueHash`), and validates the result is an ancestor of `currentDepositQueueHash` (read from Tempo state).
- **After batch**: L1 updates `lastSyncedTempoBlockNumber` to record how far Tempo state was synced.

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
2. `ZonePortal` transfers `amount` of the gas token into escrow and appends a deposit to the queue: `currentDepositQueueHash = keccak256(abi.encode(deposit, currentDepositQueueHash))`.
3. The sequencer observes `DepositMade` events and processes deposits in order via `ZoneInbox.advanceTempo()`, crediting `to` with `amount` of the gas token (TIP-20 balance). Deposits always succeed—there is no callback or bounce mechanism.
4. A batch proof/attestation must prove the zone correctly processed deposits by validating the Tempo state read inside the proof.
5. After the batch is accepted, `lastSyncedTempoBlockNumber` is updated to record how far Tempo state was synced.

Notes:

- Deposits are simple token credits. There are no callbacks or failure modes on the zone side.
- Deposits are finalized for Tempo once the batch is verified.
- There is no forced inclusion. If the sequencer withholds deposits, funds are stuck in escrow.
- The portal only stores `currentDepositQueueHash`, not individual deposits. The sequencer must track deposits off-chain.
- Tempo state advancement is combined with deposit processing in `ZoneInbox.advanceTempo()`, which calls `TempoState.finalizeTempo()` internally.
- The proof validates ancestry by reading `currentDepositQueueHash` from Tempo state, ensuring it cannot claim to process fake deposits.

## Bridging out (zone to Tempo)

Users withdraw by creating a withdrawal on the zone. Withdrawals are processed in two steps:

1. **Batch submission**: The sequencer calls `finalizeBatch()` on the zone, which constructs the withdrawal hash and emits a `BatchFinalized` event with the current `batchIndex`. The proof validates this event and adds the withdrawal hash to L1's queue.
2. **Withdrawal processing**: The sequencer calls `processWithdrawal` to process withdrawals from the oldest slot (`head`).

The `batchIndex` ensures batches are submitted in order: each batch's `batchIndex` must match the L1 portal's expected next batch. This prevents the sequencer from omitting batches that contain withdrawals.

### Withdrawal execution

When the sequencer processes a withdrawal via `processWithdrawal`, the withdrawal is **popped unconditionally** (even on failure). If the messenger call fails, funds are bounced back via a new deposit.

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

Withdrawals with `gasLimit > 0` can fail if the messenger callback reverts (out of gas, logic error, TIP-403 policy, etc.). When this happens, the portal "bounces back" the funds by re-depositing into the same zone to the withdrawal's `fallbackRecipient`.

```solidity
function _enqueueBounceBack(uint128 amount, address fallbackRecipient) internal {
    Deposit memory d = Deposit({
        sender: address(this),
        to: fallbackRecipient,
        amount: amount,
        memo: bytes32(0)
    });
    currentDepositQueueHash = keccak256(abi.encode(d, currentDepositQueueHash));
    emit BounceBack(...);
}
```

The zone processes bounce-back deposits and credits the `fallbackRecipient`. This allows withdrawals to fail gracefully without blocking the queue.

## Data availability and liveness

- Zone data availability is fully trusted to the sequencer.
- If the sequencer withholds data or halts, users cannot reconstruct zone state or force exits; batch posting and withdrawal processing are sequencer-only.
- The design assumes users accept this risk in exchange for low-cost and fast settlement.

## Bounce-back on failure

Both deposits and withdrawals can fail for various reasons. The system handles all failures gracefully via bounce-back:

### Failure reasons

Bridging operations can fail due to:
- **TIP-403 policy**: Recipient not authorized under the token's transfer policy
- **Token paused**: The gas token is globally paused
- **Callback revert**: The receiver contract reverts (out of gas, logic error, etc.)
- **Callback rejection**: Receiver returns wrong selector

### Deposit failures (zone-side)

When a deposit fails on the zone:
1. The zone cannot credit the recipient or the callback reverts
2. The zone enqueues a withdrawal back to the original `sender` on L1
3. Funds return to L1 via the normal withdrawal flow

### Withdrawal failures (L1-side)

When a withdrawal fails on L1:
1. The TIP-20 transfer or callback reverts
2. The portal enqueues a bounce-back deposit to `fallbackRecipient` on the zone
3. Funds return to zone via the normal deposit flow

### TIP-403 specific considerations

Tempo TIP-20 tokens use TIP-403 for transfer authorization:
- Every transfer checks `isAuthorized(policyId, from)` AND `isAuthorized(policyId, to)`
- Policy types: WHITELIST (must be in set) or BLACKLIST (must not be in set)
- Policy ID 1 is "always-allow" (default for most tokens)

Zone creators SHOULD choose gas tokens with `transferPolicyId == 1` to avoid complexity. If using restricted policies:
- The portal address MUST be whitelisted
- Users should set `fallbackRecipient` to an address they control

## Security considerations

- Sequencer can halt the zone without recourse due to missing data availability.
- The verifier is a trust anchor. A faulty verifier can steal or lock funds.
- Withdrawals with callbacks go through the zone messenger with a user-specified gas limit. The messenger does `transferFrom` + callback atomically; failed callbacks trigger a bounce-back to `fallbackRecipient`.
- Deposits are locked on Tempo until a verified batch consumes them.
- **Bounce-back guarantees**: Failed deposits bounce back to L1 sender; failed withdrawals bounce back to zone `fallbackRecipient`. Users always retain their funds.
- **TIP-403 policy changes**: If the gas token's policy restricts the portal, operations will fail and bounce back.
- **Token pause**: If the gas token is paused, withdrawals bounce back to zone; deposits cannot be initiated.

## Implementation architecture

This section describes the concrete implementation approach for zone nodes.

### Node architecture

Each zone runs as an ExEx (Execution Extension) attached to a Tempo L1 node. There are separate ExEx instances per zone—for example, one ExEx for a USDC zone and another for a USDT zone.

```
┌─────────────────────────────────────────────────────┐
│                  Tempo L1 Node                      │
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

- **Block hash**: Computed from the block header after execution. The block hash commits to state root, transactions root, receipts root, and other block metadata.
- **Tempo anchoring**: The zone maintains its view of Tempo state via the TempoState predeploy. Each zone block starts with a system transaction calling `ZoneInbox.advanceTempo()`, which internally calls `TempoState.finalizeTempo()` with the Tempo block header. When submitting a batch, the prover specifies a `tempoBlockNumber`, and the proof must demonstrate the zone's `tempoBlockHash` matches the actual hash from `blockhash(tempoBlockNumber)`.

### Batching and proofs

- **Batch interval**: Batches are produced every 250 milliseconds.
- **SP1 proofs**: Validity proofs are generated using Succinct's SP1 prover.
- **Mock proofs**: For development, proofs are mocked but data structures (public inputs, proof envelope) must match the real format.
- **Sequencer posting only**: Only the configured sequencer posts batch proofs to the L1 portal. The proof includes block hash and processed deposits.

```solidity
struct BatchProof {
    bytes32 nextBlockHash;
    uint64 batchIndex;            // batch index matching portal.batchIndex
    uint64 tempoBlockNumber;      // Tempo block the zone synced to
    bytes32 withdrawalQueueHash;  // hash chain of withdrawals for this batch (0 if none)
    bytes verifierConfig;         // opaque payload to IVerifier (TEE/ZK envelope)
    bytes proof;                  // SP1 proof bytes (or TEE attestation)
}
```
The portal provides `blockHash` and `batchIndex` as the previous batch's values. The proof validates that the zone's `TempoState.tempoBlockHash()` matches `blockhash(tempoBlockNumber)`, and that a `BatchFinalized` event with the correct `batchIndex` exists in the final block.

### Deposits and withdrawals

- **Deposit contract**: Tempo portal escrows TIP-20 tokens. The ExEx watches `DepositMade` events and queues deposits for zone processing.
- **Combined system transaction**: Each zone block starts with a system transaction that calls `ZoneInbox.advanceTempo(header, deposits)`. This atomically advances the zone's Tempo view and processes pending deposits, validating the deposit hash against Tempo state.
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
Tempo L1 Node
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
