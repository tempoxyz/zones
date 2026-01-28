# Tempo Zones (Draft)

This document proposes a new validium protocol designed for Tempo. It is a design overview, not a full specification.

## Goals

- Create a Tempo-native validium called a zone.
- Each zone has exactly one permissioned sequencer.
- Each zone bridges exactly one TIP-20 token, which is also the zone gas token.
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
- Gas token: the zone's only TIP-20, bridged from Tempo.
- Portal: the Tempo-side contract that escrows the gas token and finalizes exits.
- Batch: a sequencer-produced commitment covering one or more zone blocks. The batch **must** end with a single `finalizeWithdrawalBatch()` call in the final block, and intermediate blocks **must not** call `finalizeWithdrawalBatch()`. The sequencer controls batch frequency.

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
- `zoneParams`: initial configuration (genesis block hash, genesis Tempo block hash/number).

The factory deploys a `ZonePortal` that escrows the gas token on Tempo. The zone genesis includes the portal address and the gas token configuration.

### Sequencer transfer

The sequencer can transfer control to a new address via a two-step process:

1. Current sequencer calls `transferSequencer(newSequencer)` to nominate a new sequencer
2. New sequencer calls `acceptSequencer()` to accept the transfer

This applies to all zone contracts: `ZonePortal` (Tempo-side), `ZoneInbox`, `ZoneOutbox`, and `TempoState` (zone-side). The two-step pattern prevents accidental transfers to incorrect addresses.

## Execution and fees

- The zone reuses Tempo's fee units and accounting model.
- The fee token is always the gas token. There is no fee token selection.
- Transactions use Tempo transaction semantics for fee payer, max fee per gas, and gas limit. The fee token field is fixed to the gas token.

### Deposit fees

> **TODO**: Deposit fee mechanism is undecided. Options include: minimum deposit amount (spam prevention), fixed fee deducted from deposit, or no fee (sequencer absorbs zone-side gas costs). Zone gas should be cheap due to no data availability costs.

### Withdrawal processing fees

Withdrawals incur a processing fee to compensate the sequencer for Tempo-side gas costs:

- **Base fee**: Fixed cost per withdrawal (covers `processWithdrawal` overhead)
- **Gas fee**: Proportional to `gasLimit` (covers callback execution)
- **Total fee**: `baseFee + gasLimit * gasFeeRate`

The sequencer configures these parameters via `ZoneOutbox.setWithdrawalFees(baseFee, gasFeeRate)`. The fee is calculated and locked in at request time, stored in the `Withdrawal.fee` field, and paid to the sequencer when the withdrawal is processed on Tempo (regardless of success or failure).

Users burn `amount + fee` when requesting a withdrawal. On success, `amount` goes to the recipient and `fee` goes to the sequencer. On failure (bounce-back), only `amount` is re-deposited to `fallbackRecipient`; the sequencer keeps the fee.

## Batch submission

The sequencer posts batches to Tempo via a single `submitBatch` call (sequencer-only) that:

1. Verifies the proof/attestation for the state transition (including chain integrity via `prevBlockHash`).
2. Updates the portal's `withdrawalBatchIndex`, `blockHash`, and `lastSyncedTempoBlockNumber`.
3. Updates the withdrawal queue (adds new withdrawals to the next slot in the unbounded buffer).

Each batch submission includes:

- `tempoBlockNumber` (the Tempo block number for EIP-2935 block hash history verification)
- `blockTransition` (contains `prevBlockHash` and `nextBlockHash` - the zone block hash transition)
- `depositQueueTransition` (contains `prevProcessedHash` and `nextProcessedHash` - deposit queue processing)
- `withdrawalQueueTransition` (contains `withdrawalQueueHash` - hash chain of withdrawals for this batch, or 0 if none)
- `verifierConfig` (opaque payload forwarded to the verifier for domain separation/attestation needs)
- `proof` (validity proof or TEE attestation)

The portal tracks `withdrawalBatchIndex`, `blockHash` (the last proven batch block), `lastSyncedTempoBlockNumber` (the Tempo block the zone has synced to), `currentDepositQueueHash` (head of deposit queue), and an unbounded buffer for withdrawals with `head`, `tail`, and `maxSize` indices.

If `tempoBlockNumber` falls outside the EIP-2935 history window, batch submission must use a recursive proof or checkpointed proof that anchors to a newer block (TODO).

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

/// @notice Withdrawal queue transition for batch proofs
struct WithdrawalQueueTransition {
    bytes32 withdrawalQueueHash;  // hash chain of withdrawals for this batch (0 if none)
}

interface IVerifier {
    /// @notice Verify a batch proof
    /// @dev The proof validates:
    ///      1. Valid state transition from prevBlockHash to nextBlockHash
    ///      2. Zone's TempoState.tempoBlockHash() matches tempoBlockHash for tempoBlockNumber
    ///      3. ZoneOutbox.lastBatch().withdrawalBatchIndex == expectedWithdrawalBatchIndex
    ///      4. ZoneOutbox.lastBatch().withdrawalQueueHash matches withdrawalQueueTransition
    ///      5. Zone block beneficiary matches sequencer
    ///      6. Deposit processing is correct (validated via Tempo state read inside proof)
    function verify(
        uint64 tempoBlockNumber,
        bytes32 tempoBlockHash,
        uint64 expectedWithdrawalBatchIndex,
        address sequencer,
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
2. The zone's `TempoState.tempoBlockHash()` matches the `tempoBlockHash` provided by the portal for `tempoBlockNumber`.
3. The zone's `TempoState.tempoBlockNumber()` matches `tempoBlockNumber`.
4. The `ZoneOutbox.lastBatch()` storage contains the correct `withdrawalBatchIndex` and `withdrawalQueueHash`.
5. Deposit processing is correct: the zone read `currentDepositQueueHash` from Tempo state and processed deposits accordingly.
6. The zone block `beneficiary` matches the registered sequencer.

The zone has access to Tempo state via the TempoState predeploy, so the proof can read `currentDepositQueueHash` directly from Tempo storage at the proven block. This eliminates the need for an on-chain "ceiling" slot.

`verifierData` + `proof` are opaque to the portal: ZK systems can ignore `verifierData`, while TEEs can pack attestation envelopes/quotes and measurement checks into `verifierData` for the verifier contract to enforce.

`submitBatch` verifies that `prevBlockHash == blockHash`, then calls the verifier. On success it updates `withdrawalBatchIndex`, `blockHash`, `lastSyncedTempoBlockNumber`, adds withdrawals to the queue, and emits `BatchSubmitted` with `withdrawalBatchIndex`, `nextProcessedDepositQueueHash`, `nextBlockHash`, and `withdrawalQueueHash` for off-chain auditing.

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
/// @notice Interface for the zone's gas token (TIP-20 with mint/burn for system)
interface IZoneGasToken {
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
    address token;
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

struct Deposit {
    address sender;
    address to;
    uint128 amount;
    bytes32 memo;
}

struct Withdrawal {
    address sender;             // who initiated the withdrawal on the zone
    address to;                 // Tempo recipient
    uint128 amount;             // amount to send to recipient (excludes fee)
    uint128 fee;                // processing fee for sequencer (calculated at request time)
    bytes32 memo;
    uint64 gasLimit;            // max gas for IWithdrawalReceiver callback (0 = no callback)
    address fallbackRecipient;  // zone address for bounce-back if call fails
    bytes callbackData;         // calldata for IWithdrawalReceiver (if gasLimit > 0, max 1KB)
}
```

### Verifier

```solidity
interface IVerifier {
    function verify(
        uint64 tempoBlockNumber,
        bytes32 tempoBlockHash,
        uint64 expectedWithdrawalBatchIndex,
        address sequencer,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
    ) external view returns (bool);
}
```

The verifier receives the `tempoBlockNumber` and `tempoBlockHash` (looked up on-chain via the EIP-2935 block hash history precompile), `expectedWithdrawalBatchIndex` (portal's current batch index + 1), `sequencer` (the registered sequencer address), block transition, deposit queue transition, withdrawal queue transition, and the proof. The proof must demonstrate that the zone's internal Tempo view matches `tempoBlockHash` for `tempoBlockNumber`, that the state transition is valid, that `ZoneOutbox.lastBatch().withdrawalBatchIndex` equals `expectedWithdrawalBatchIndex`, that the zone block `beneficiary` matches `sequencer`, and that `ZoneOutbox.lastBatch().withdrawalQueueHash` matches `withdrawalQueueTransition.withdrawalQueueHash` (read from state root, not events).

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

#### WithdrawalQueueLib (unbounded buffer)

Handles L2→Tempo withdrawals where the producer (proof) is slow and the consumer (on-chain) is fast. Each batch gets its own slot in an unbounded buffer.

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

**Gas note**: Storage gas should only be charged when `(tail - head) > maxSize`. This is enforced by the precompile implementation.

| Queue | Tempo operation | Zone/Proof operation |
|-------|--------------|---------------------|
| Deposit | `enqueue` (users deposit) | Process via `advanceTempo()` |
| Withdrawal | `dequeue` (sequencer processes) | Create via `finalizeWithdrawalBatch()` |

### Tempo contracts

#### Zone factory

```solidity
interface IZoneFactory {
    struct CreateZoneParams {
        address token;
        address sequencer;
        address verifier;
        ZoneParams zoneParams;
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

    function zoneId() external view returns (uint64);
    function token() external view returns (address);
    function messenger() external view returns (address);
    function sequencer() external view returns (address);
    function pendingSequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
    function verifier() external view returns (address);
    function genesisTempoBlockNumber() external view returns (uint64);
    function withdrawalBatchIndex() external view returns (uint64);
    function blockHash() external view returns (bytes32);
    function currentDepositQueueHash() external view returns (bytes32);
    function lastSyncedTempoBlockNumber() external view returns (uint64);
    function withdrawalQueueHead() external view returns (uint256);
    function withdrawalQueueTail() external view returns (uint256);
    function withdrawalQueueMaxSize() external view returns (uint256);
    function withdrawalQueueSlot(uint256 slot) external view returns (bytes32);

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    function transferSequencer(address newSequencer) external;

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external;

    /// @notice Set the sequencer's public key. Only callable by the sequencer.
    function setSequencerPubkey(bytes32 pubkey) external;

    /// @notice Deposit gas token into the zone. Returns the new current deposit queue hash.
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositQueueHash);

    /// @notice Process the next withdrawal from the queue. Only callable by the sequencer.
    /// @dev Fee is paid to sequencer regardless of success/failure.
    /// @param withdrawal The withdrawal to process (must be at the head of the current slot).
    /// @param remainingQueue The hash of the remaining withdrawals in this slot (0 if last).
    function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external;

    /// @notice Submit a batch and verify the proof. Only callable by the sequencer.
    /// @dev Verifies prevBlockHash == blockHash, then calls the verifier.
    ///      On success updates withdrawalBatchIndex, blockHash, lastSyncedTempoBlockNumber,
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

#### Zone messenger (Tempo)

Each zone has a dedicated messenger contract on Tempo. The portal gives the messenger max approval for the gas token. Withdrawal callbacks originate from this contract, not the portal.

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
    /// @param target The Tempo recipient
    /// @param amount Tokens to transfer from portal to target
    /// @param gasLimit Max gas for the callback
    /// @param data Calldata for the target
    function relayMessage(
        address sender,
        address target,
        uint128 amount,
        uint64 gasLimit,
        bytes calldata data
    ) external;
}
```

The messenger does `token.transferFrom(portal, target, amount)` then calls the target with `data`. Both are atomic: if the callback reverts, the transfer is also reverted. Receivers check `msg.sender == zoneMessenger` and call `zoneMessenger.xDomainMessageSender()` to authenticate the L2 origin.

#### Withdrawal receiver

Contracts that receive withdrawals with callbacks must implement this interface:

```solidity
interface IWithdrawalReceiver {
    /// @notice Called when a withdrawal with callback is received
    /// @param sender The L2 origin address
    /// @param amount The amount of tokens transferred
    /// @param callbackData The callback data from the withdrawal request
    /// @return The function selector to confirm successful handling
    function onWithdrawalReceived(
        address sender,
        uint128 amount,
        bytes calldata callbackData
    ) external returns (bytes4);
}
```

The receiver must return `IWithdrawalReceiver.onWithdrawalReceived.selector` to confirm successful handling. If the receiver reverts or returns the wrong selector, the withdrawal fails and bounces back.

### Zone predeploys

#### Zone gas token

The zone's gas token is the bridged TIP-20 from Tempo. It is deployed at the **same address** on the zone as on Tempo. Users interact with it via the standard TIP-20 interface for transfers and approvals. The zone sequencer mints tokens when processing deposits and burns them when withdrawals are requested.

#### TempoState predeploy

The TempoState predeploy allows zones to verify they have a correct view of Tempo state. It stores the Tempo wrapper fields and selected inner Ethereum header fields, and provides storage reading functionality.

```solidity
// Predeploy address: 0x1c00000000000000000000000000000000000000
interface ITempoState {
    event TempoBlockFinalized(bytes32 indexed blockHash, uint64 indexed blockNumber, bytes32 stateRoot);
    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    /// @notice Current sequencer address
    function sequencer() external view returns (address);

    /// @notice Pending sequencer for two-step transfer
    function pendingSequencer() external view returns (address);

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

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    function transferSequencer(address newSequencer) external;

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external;

    /// @notice Finalize a Tempo block header. Only callable by sequencer.
    /// @dev Validates chain continuity (parent hash must match, number must be +1)
    /// @param header RLP-encoded Tempo header
    function finalizeTempo(bytes calldata header) external;

    /// @notice Read a storage slot from a Tempo contract
    function readTempoStorageSlot(address account, bytes32 slot) external view returns (bytes32);

    /// @notice Read multiple storage slots from a Tempo contract
    function readTempoStorageSlots(address account, bytes32[] calldata slots) external view returns (bytes32[] memory);
}
```

Tempo headers are RLP encoded as `rlp([general_gas_limit, shared_gas_limit, timestamp_millis_part, inner])`, where `inner` is a standard Ethereum header. The inner header uses trailing-field semantics for optional fields: `baseFeePerGas` (EIP-1559), `withdrawalsRoot` (EIP-4895), `blobGasUsed`/`excessBlobGas` (EIP-4844), `parentBeaconBlockRoot` (EIP-4788), and `requestsHash` (EIP-7685). TempoState skips these trailing optional fields and does not expose them.

The TempoState stores the Tempo wrapper fields and the inner fields needed by the zone/proof logic; the `tempoBlockHash` is always `keccak256(RLP(TempoHeader))`, so proofs still commit to the complete header contents.

**How it works:**

1. The sequencer submits Tempo block headers via `finalizeTempo()`, which decodes the RLP header, validates chain continuity, and stores the wrapper fields and selected inner fields.
2. When submitting a batch, the prover specifies a `tempoBlockNumber`. The portal reads the hash via the EIP-2935 block hash history precompile.
3. The proof must demonstrate that the zone's `tempoBlockHash` (from TempoState) matches the value passed by the portal.
4. The `readTempoStorageSlot` functions are precompile stubs - actual implementation is in the zone node, validated against `tempoStateRoot`.

Tempo state staleness depends on how frequently the sequencer calls `finalizeTempo()`. The zone client must only finalize Tempo headers after finality; proofs should only reference finalized Tempo blocks to avoid reorg risk. The prover includes Merkle proofs for each unique account and storage slot accessed during the batch.

#### Tempo state declarations (transaction type `0x7A`)

Zone transactions that read Tempo state must include a **Tempo state declaration**—an extension of EIP-2930 access lists that includes the actual values. This enables the sequencer to validate transactions without blocking on Tempo state fetches.

**Format (analogous to EIP-2930):**

| EIP-2930 Access List | Tempo State Declaration |
|---------------------|-------------------------|
| `[[address, [slot, ...]], ...]` | `[[address, [[slot, value], ...]], ...]` |

```solidity
/// @notice A single storage slot and its declared value
struct TempoStorageEntry {
    bytes32 slot;   // Storage slot key
    bytes32 value;  // Declared value at this slot
}

/// @notice Tempo state for a single account (contract)
struct TempoAccountState {
    address account;                    // Tempo contract address
    TempoStorageEntry[] storageEntries; // Declared storage slots and values
}

/// @notice Complete Tempo state declaration for a transaction
struct TempoStateDeclaration {
    TempoAccountState[] accountStates;  // Declared state per account
}
```

**Transaction type:** `0x7A` ('z' for zone)

Transactions using Tempo state declarations use a new transaction type. The format extends EIP-1559 (type 2) transactions:

```
0x7A || rlp([chainId, nonce, maxPriorityFeePerGas, maxFeePerGas, gasLimit, to, value, data, accessList, tempoStateDeclaration, signatureYParity, signatureR, signatureS])
```

Where `tempoStateDeclaration` is:

```
[[[account1, [[slot1, value1], [slot2, value2], ...]], [account2, [[slot1, value1], ...]], ...]]
```

**Validation rules:**

1. All declared values must match current Tempo state (at the zone's finalized Tempo block)
2. Execution must not read any Tempo state not covered by the declaration
3. Transactions that fail either check are **invalid** and cannot be included in a block (the zone block itself becomes invalid)

By not including a block number in the declaration, transactions remain valid as long as the declared values match current Tempo state. They only become invalid when the actual state changes, not just because a new Tempo block was finalized. This makes transactions more robust and reduces unnecessary invalidation.

**Gas costs (similar to EIP-2930):**

| Operation | Gas Cost |
|-----------|----------|
| Declare account | 2,400 |
| Declare storage slot | 1,900 |
| Read declared slot during execution | 100 (warm read) |

**Benefits:**

- **Non-blocking mempool**: The sequencer can validate and order transactions without fetching Tempo state synchronously
- **User responsibility**: Wallets/dapps fetch and declare state, shifting work off the critical path
- **Early rejection**: Invalid declarations are rejected before execution, saving gas
- **Proof efficiency**: The batch proof only needs to verify the declared state matches Tempo, not trace all storage accesses

**Example usage:**

A transaction calling `TIP403Registry.isAuthorized()` (which reads Tempo state internally) must declare the policy storage slots it will access:

```javascript
const tx = {
  type: 0x7a,
  chainId: zoneChainId,
  nonce: 0,
  maxPriorityFeePerGas: 0,
  maxFeePerGas: 1000000000,
  gasLimit: 100000,
  to: tip403RegistryAddress,
  value: 0,
  data: isAuthorizedCalldata,
  accessList: [],
  tempoStateDeclaration: {
    accountStates: [{
      account: tempoPolicyContractAddress,
      storageEntries: [
        { slot: policySlot, value: policyValue }
      ]
    }]
  }
};
```

#### TIP-403 registry

The zone has a `TIP403Registry` contract deployed at the **same address** as Tempo. This contract is read-only—it does not support writing policies. Its `isAuthorized` function reads policy state from Tempo via the Tempo state reader precompile, so zone-side TIP-20 transfers enforce Tempo TIP-403 policies automatically.

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

    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    /// @notice The Tempo portal address (for reading deposit queue hash).
    function tempoPortal() external view returns (address);

    /// @notice The TempoState predeploy address.
    function tempoState() external view returns (TempoState);

    /// @notice The gas token (TIP-20 at same address as Tempo).
    function gasToken() external view returns (IZoneGasToken);

    /// @notice Current sequencer address.
    function sequencer() external view returns (address);

    /// @notice Pending sequencer for two-step transfer.
    function pendingSequencer() external view returns (address);

    /// @notice The zone's last processed deposit queue hash.
    function processedDepositQueueHash() external view returns (bytes32);

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    function transferSequencer(address newSequencer) external;

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external;

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

The zone outbox handles withdrawal requests. Users approve the outbox to spend their gas tokens, then call `requestWithdrawal`. The outbox stores pending withdrawals in an array. When the sequencer is ready to finalize a **batch**, it calls `finalizeWithdrawalBatch(count)` as a system transaction at the end of the **final block** in that batch. This constructs the withdrawal queue hash on-chain and writes the `withdrawalQueueHash` and `withdrawalBatchIndex` to storage. Intermediate blocks **must not** call `finalizeWithdrawalBatch()`. The call is required even if there are zero withdrawals (use `count = 0`) so the withdrawal batch index advances. The event is emitted for observability, but the proof reads from state (via the `lastBatch` storage) rather than parsing event logs.

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
        address to,
        uint128 amount,
        uint128 fee,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes data
    );

    event WithdrawalFeesUpdated(uint128 baseFee, uint128 gasFeeRate);

    /// @notice Emitted when sequencer finalizes a batch at end of block.
    /// @dev Kept for observability. Proof reads from lastBatch storage instead.
    event BatchFinalized(
        bytes32 indexed withdrawalQueueHash,
        uint64 withdrawalBatchIndex
    );

    event SequencerTransferStarted(address indexed currentSequencer, address indexed pendingSequencer);
    event SequencerTransferred(address indexed previousSequencer, address indexed newSequencer);

    /// @notice The gas token (same as Tempo portal's token).
    function gasToken() external view returns (IZoneGasToken);

    /// @notice Current sequencer address.
    function sequencer() external view returns (address);

    /// @notice Pending sequencer for two-step transfer.
    function pendingSequencer() external view returns (address);

    /// @notice Base fee for withdrawal processing.
    function withdrawalBaseFee() external view returns (uint128);

    /// @notice Fee per unit of gasLimit.
    function withdrawalGasFeeRate() external view returns (uint128);

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

    /// @notice Set withdrawal fee parameters. Only callable by sequencer.
    function setWithdrawalFees(uint128 baseFee, uint128 gasFeeRate) external;

    /// @notice Calculate the fee for a withdrawal with the given gasLimit.
    function calculateWithdrawalFee(uint64 gasLimit) external view returns (uint128);

    /// @notice Request a withdrawal from the zone back to Tempo.
    /// @dev Caller must have approved the outbox to spend `amount + fee` of gas tokens.
    ///      Tokens are burned immediately and withdrawal is stored in pending array.
    /// @param to The Tempo recipient address.
    /// @param amount Amount to send to recipient (fee is additional).
    /// @param memo User-provided context (e.g., payment reference).
    /// @param gasLimit Gas limit for messenger callback (0 = no callback).
    /// @param fallbackRecipient Zone address for bounce-back if callback fails.
    /// @param data Calldata for the target (max 1KB).
    function requestWithdrawal(
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes calldata data
    ) external;

    /// @notice Finalize batch at end of final block - build withdrawal hash and write to state.
    /// @dev Only callable by sequencer as a system transaction in the final block of a batch.
    ///      Must be called exactly once per batch (count may be 0). Writes `withdrawalQueueHash`
    ///      and `withdrawalBatchIndex` to lastBatch storage for proof access.
    /// @param count Max number of withdrawals to process (avoids unbounded loops).
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
- The proof validates an exact match to `currentDepositQueueHash` from Tempo state, ensuring it cannot claim to process fake deposits. TODO: implement a recursive ancestor check in the proof or on-chain as a fallback.

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

Withdrawals can fail if the token transfer or messenger callback reverts (out of gas, logic error, TIP-403 policy, token pause, etc.). When this happens, the portal "bounces back" the funds by re-depositing into the same zone to the withdrawal's `fallbackRecipient`.

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

## Withdrawal failure details

Withdrawals can fail for various reasons. The system handles failures gracefully via bounce-back:

### Failure reasons

Withdrawals can fail due to:
- **Transfer failure**: `transfer` or `transferFrom` reverts (includes gasLimit = 0 cases)
- **TIP-403 policy**: Recipient not authorized under the token's transfer policy
- **Token paused**: The gas token is globally paused
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

Zone creators SHOULD choose gas tokens with `transferPolicyId == 1` to avoid complexity. If using restricted policies:
- The portal address MUST be whitelisted
- Users should set `fallbackRecipient` to an address they control

## Security considerations

- Sequencer can halt the zone without recourse due to missing data availability.
- The verifier is a trust anchor. A faulty verifier can steal or lock funds.
- Withdrawals with callbacks go through the zone messenger with a user-specified gas limit. The messenger does `transferFrom` + callback atomically; any transfer or callback failure triggers a bounce-back to `fallbackRecipient`.
- Deposits are locked on Tempo until a verified batch consumes them.
- **Bounce-back guarantees**: Failed withdrawals bounce back to zone `fallbackRecipient`. Users always retain their funds.
- **TIP-403 policy changes**: If the gas token's policy restricts the portal, withdrawals will fail and bounce back.
- **Token pause**: If the gas token is paused, withdrawals bounce back to zone.

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
- **Transactions/receipts roots**: Computed over the full ordered list `[advanceTempo, user txs..., finalizeWithdrawalBatch?]`.
- **Transactions root**: Committed in the block hash but not proven on-chain. This prevents sequencer revisionism (claiming different transactions led to the state) while avoiding expensive transaction proof verification.
- **Receipts root**: Committed in the block hash but not proven on-chain. Batch parameters are read from `lastBatch` state storage instead of event logs.
- **Tempo anchoring**: The zone maintains its view of Tempo state via the TempoState predeploy. Each zone block starts with a system transaction calling `ZoneInbox.advanceTempo()`, which internally calls `TempoState.finalizeTempo()` with the Tempo block header. When submitting a batch, the prover specifies a `tempoBlockNumber`, and the proof must demonstrate the zone's `tempoBlockHash` matches the actual hash from the EIP-2935 history precompile.

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
