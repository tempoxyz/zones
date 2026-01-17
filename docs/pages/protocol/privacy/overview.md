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
- Batch: a sequencer-produced commitment to a single zone block. There is a 1:1 relationship between batches and blocks.

## System overview

### Actors

- Zone sequencer: permissioned operator that orders zone transactions, provides data, and posts batches to Tempo. The sequencer is the only actor that submits transactions to the portal.
- Verifier: ZK proof system or TEE attester. Abstracted via `IVerifier`.
- Users: deposit TIP-20 from Tempo to the zone or exit back to Tempo.

### Tempo contracts

- `ZoneFactory`: creates zones and registers parameters.
- `ZonePortal`: per-zone portal that escrows the gas token on Tempo and finalizes exits.
- `ZoneRegistry`: optional registry for zone metadata and active batch head.

### Zone components (off-chain or zone-side)

- `ZoneSequencer`: collects transactions and creates batches.
- `ZoneExecutor`: executes the zone state transition.
- `ZoneProver` or `ZoneAttester`: produces proof/attestation for each batch.

## Zone creation

A zone is created via `ZoneFactory.createZone(...)` with:

- `token`: the Tempo TIP-20 address to bridge. This is the only bridged token and the gas token.
- `sequencer`: permissioned sequencer address.
- `verifier`: `IVerifier` implementation for proof or attestation.
- `zoneParams`: initial configuration (genesis state root, fee parameters).

The factory deploys a `ZonePortal` that escrows the gas token on Tempo. The zone genesis includes the portal address and the gas token configuration.

## Execution and fees

- The zone reuses Tempo's fee units and accounting model.
- The fee token is always the gas token. There is no fee token selection.
- Transactions use Tempo transaction semantics for fee payer, max fee per gas, and gas limit. The fee token field is fixed to the gas token.

## Batch submission

The sequencer posts batches to Tempo via a single `submitBatch` call (sequencer-only) that:

1. Verifies the proof/attestation for the state transition.
2. Updates the portal's state root.
3. Updates the withdrawal queue (adds new withdrawals to `pendingWithdrawalQueueHash`).

Each batch submission includes:

- `verifierData` (opaque payload forwarded to the verifier for domain separation/attestation needs)
- `nextProcessedDepositQueueHash` (the deposit queue messages processed up to)
- `nextStateRoot` (the resulting state after execution)
- `prevPendingWithdrawalQueueHash` (the pending withdrawal queue hash the proof assumed)
- `nextPendingWithdrawalQueueHashIfFull` (pending queue with new withdrawals if `prevPendingWithdrawalQueueHash` matches)
- `nextPendingWithdrawalQueueHashIfEmpty` (new withdrawals only, if pending was swapped during proving)
- `proof` (validity proof or TEE attestation)

The portal tracks `stateRoot`, `processedDepositQueueHash` (where proofs start from), `snapshotDepositQueueHash` (stable target for proofs), `currentDepositQueueHash` (head of deposit queue), `activeWithdrawalQueueHash` (active queue), and `pendingWithdrawalQueueHash` (pending queue).

The portal calls the verifier to validate the batch:

```solidity
/// @notice State transition for zone batch proofs
struct StateTransition {
    bytes32 prevStateRoot;
    bytes32 nextStateRoot;
}

/// @notice Deposit queue transition inputs/outputs for batch proofs
struct DepositQueueTransition {
    bytes32 prevSnapshotHash;      // stable target ceiling
    bytes32 prevProcessedHash;     // where proof starts
    bytes32 nextProcessedHash;     // where zone processed up to
}

/// @notice Withdrawal queue transition inputs/outputs for batch proofs
struct WithdrawalQueueTransition {
    bytes32 prevPendingHash;         // what proof assumed pending queue was
    bytes32 nextPendingHashIfFull;   // pending queue after append if no swap
    bytes32 nextPendingHashIfEmpty;  // pending queue after append if swap occurred
}

interface IVerifier {
    function verify(
        StateTransition calldata stateTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierData,
        bytes calldata proof
    ) external view returns (bool);
}
```

The verifier validates that:
1. The state transition from `prevStateRoot` to `nextStateRoot` is correct given the processed deposit queue messages.
2. `nextProcessedDepositQueueHash` is a descendant of `processedDepositQueueHash` (messages were processed forward).
3. `nextProcessedDepositQueueHash` is an ancestor of `snapshotDepositQueueHash` (no fake messages beyond the snapshot).

`verifierData` + `proof` are opaque to the portal: ZK systems can ignore `verifierData`, while TEEs can pack attestation envelopes/quotes and measurement checks into `verifierData` for the verifier contract to enforce.

`submitBatch` passes the portal's current `stateRoot`, `processedDepositQueueHash`, and `snapshotDepositQueueHash` as verifier inputs (prev values), and reverts unless `prevStateRoot == stateRoot` and the verifier approves. On success it increments `batchIndex`, updates roots/queues, and emits `BatchSubmitted` with every verifier input/output (except the proof bytes) so off-chain observers can audit the batch.

### Deposit queue

Tempo to zone communication uses a single `depositQueue` chain. Each message is either:

- **Deposit**: user deposit, including L1 block info + deposit data
- **L1Sync**: sequencer-triggered L1 head sync, including L1 block info only

Messages are hashed into a chain:

```
newHash = keccak256(abi.encode(message, prevHash))
```

Where `message` is `DepositQueueMessage { kind, data }` and `data` is `abi.encode(Deposit)` or `abi.encode(L1Sync)`.

Deposits and L1 syncs both carry L1 block info (parent block hash, block number, timestamp), so the zone updates its L1 view in-order with other deposit queue messages. This enables the L1 state reader precompile to access L1 storage.

The portal uses a 3-slot design to allow partial processing while maintaining on-chain verifiability:

| Slot | Name | Role |
|------|------|------|
| 1 | `processedDepositQueueHash` | Where proofs start (last proven state) |
| 2 | `snapshotDepositQueueHash` | Stable target ceiling for the current proof |
| 3 | `currentDepositQueueHash` | Head of chain (new messages land here) |

**Proof requirements**: The proof must verify that the zone correctly processed deposit queue messages from `processedDepositQueueHash` to `nextProcessedDepositQueueHash`, and that `nextProcessedDepositQueueHash` is an ancestor of `snapshotDepositQueueHash`. This ancestry check happens inside the proof—the prover includes the unprocessed messages between `nextProcessedDepositQueueHash` and `snapshotDepositQueueHash` and verifies their hashes chain correctly, without executing them.

**After batch accepted**:
1. `processedDepositQueueHash = nextProcessedDepositQueueHash` (advance to where we actually processed)
2. `snapshotDepositQueueHash = currentDepositQueueHash` (snapshot new target for next proof)

This is the depositQueue-side equivalent of the two-queue withdrawal system: slots 1->2 are proven, slot 3 accumulates new messages, then rotate.

Proofs or attestations are assumed to be fast. No data availability is required by the verifier.

## Withdrawal queue

Withdrawals use a two-queue system that allows the sequencer to process withdrawals independently of proof generation. The portal tracks two hash chains in constant space (2 storage slots):

- `activeWithdrawalQueueHash` - the active queue, processed by the sequencer
- `pendingWithdrawalQueueHash` - the pending queue, updated by proofs

### Hash chain structure

Each queue is a hash chain with the **oldest withdrawal at the outermost layer**, making FIFO processing efficient:

```
queue = keccak256(abi.encode(w1, keccak256(abi.encode(w2, keccak256(abi.encode(w3, bytes32(0)))))))
      // w1 is oldest (outermost), w3 is newest (innermost)
```

To process the oldest withdrawal, the sequencer provides the withdrawal data and the remaining queue hash. The portal verifies the hash and advances the queue:

```solidity
function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external onlySequencer {
    // If active is empty, swap in pending first
    if (activeWithdrawalQueueHash == bytes32(0)) {
        require(pendingWithdrawalQueueHash != bytes32(0), "no withdrawals");
        activeWithdrawalQueueHash = pendingWithdrawalQueueHash;
        pendingWithdrawalQueueHash = bytes32(0);
    }

    require(keccak256(abi.encode(w, remainingQueue)) == activeWithdrawalQueueHash, "invalid");

    _executeWithdrawal(w);

    if (remainingQueue == bytes32(0)) {
        // Active exhausted, swap in pending
        activeWithdrawalQueueHash = pendingWithdrawalQueueHash;
        pendingWithdrawalQueueHash = bytes32(0);
    } else {
        activeWithdrawalQueueHash = remainingQueue;
    }
}
```

### Proof updates to the pending queue

When a proof is submitted, it adds new withdrawals to `pendingWithdrawalQueueHash`. The proof builds the queue with new withdrawals at the **innermost** layers (newest = last to process). This is an O(N) operation but happens inside the ZKP.

### Race condition handling

A race condition can occur:
1. Proof generation starts when `pendingWithdrawalQueueHash = X` (non-empty)
2. Meanwhile, sequencer drains `activeWithdrawalQueueHash`, triggering swap: `activeWithdrawalQueueHash = X`, `pendingWithdrawalQueueHash = 0`
3. Proof submits expecting `pendingWithdrawalQueueHash = X`, but it's now `0`

To handle this, the proof generates two outputs:
- `nextPendingWithdrawalQueueHashIfFull` - new withdrawals added to innermost of the expected pending queue
- `nextPendingWithdrawalQueueHashIfEmpty` - new withdrawals as a fresh queue (as if pending was empty)

The caller provides `prevPendingHash` in `WithdrawalQueueTransition` (what the proof assumed), and the portal uses the appropriate value:

```solidity
function submitBatch(
    StateTransition calldata stateTransition,
    DepositQueueTransition calldata depositQueueTransition,
    WithdrawalQueueTransition calldata withdrawalQueueTransition,
    bytes calldata verifierData,
    bytes calldata proof
) external onlySequencer {
    // ... verify proof ...

    if (pendingWithdrawalQueueHash == prevPendingWithdrawalQueueHash) {
        pendingWithdrawalQueueHash = nextPendingWithdrawalQueueHashIfFull;
    } else if (pendingWithdrawalQueueHash == bytes32(0)) {
        pendingWithdrawalQueueHash = nextPendingWithdrawalQueueHashIfEmpty;
    } else {
        revert("unexpected pending queue");
    }
}
```

This ensures the proof works regardless of whether a queue swap happened during proving.

## Interfaces and functions

This section defines the functions and interfaces used by the design. The signatures are Solidity-style and focus on the minimum surface area.

### Common types

```solidity

struct ZoneInfo {
    uint64 zoneId;
    address portal;
    address messenger;      // L1 messenger for this zone
    address token;
    address sequencer;
    address verifier;
    bytes32 genesisStateRoot;
}

enum DepositQueueMessageKind {
    Deposit,
    L1Sync
}

struct L1Sync {
    bytes32 l1ParentBlockHash;
    uint64 l1BlockNumber;
    uint64 l1Timestamp;
}

struct DepositQueueMessage {
    DepositQueueMessageKind kind;
    bytes data; // abi.encode(Deposit) or abi.encode(L1Sync)
}

struct Deposit {
    // L1 block info (zone receives L1 state through deposit queue messages)
    bytes32 parentBlockHash;    // blockhash(block.number - 1) - used for L1 state reader
    uint64 blockNumber;         // block.number
    uint64 timestamp;           // block.timestamp
    // Deposit data
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
    uint64 gasLimit;            // max gas for messenger callback (0 = no callback)
    address fallbackRecipient;  // zone address for bounce-back if call fails
    bytes data;                 // calldata for target (if gasLimit > 0)
}
```

### Verifier

```solidity
interface IVerifier {
    function verify(
        StateTransition calldata stateTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierData,
        bytes calldata proof
    ) external view returns (bool);
}
```

### Queue libraries

The portal uses two queue libraries that encapsulate the hash chain management patterns:

#### DepositQueueLib (3-slot ceiling pattern)

Handles L1→L2 deposits where the producer (L1) is fast and the consumer (proof) is slow.

```solidity
struct DepositQueue {
    bytes32 processed;  // where proofs start (last proven state)
    bytes32 snapshot;   // stable target ceiling for current proof
    bytes32 current;    // head of queue (new messages land here)
}

library DepositQueueLib {
    /// @notice Enqueue a new message into the queue (on-chain operation)
    /// @dev Hash chain: newHash = keccak256(abi.encode(message, prevHash))
    function enqueue(DepositQueue storage q, DepositQueueMessage memory message) internal returns (bytes32 newHeadQueueHash);

    /// @notice Dequeue deposits via proof (proof operation)
    /// @dev Validates expected state matches actual, then updates:
    ///      processed = nextProcessed, snapshot = current
    function dequeueWithProof(
        DepositQueue storage q,
        DepositQueueTransition memory transition
    ) internal;
}
```

#### WithdrawalQueueLib (2-slot swap pattern)

Handles L2→L1 withdrawals where the producer (proof) is slow and the consumer (on-chain) is fast.

```solidity
struct WithdrawalQueue {
    bytes32 active;   // active queue (being drained on-chain)
    bytes32 pending;  // pending queue (being filled by proofs)
}

library WithdrawalQueueLib {
    /// @notice Dequeue the next withdrawal from the queue (on-chain operation)
    /// @dev Verifies keccak256(abi.encode(w, remainingQueue)) == active
    ///      Automatically swaps in pending if active is empty
    function dequeue(WithdrawalQueue storage q, Withdrawal calldata w, bytes32 remainingQueue) internal;

    /// @notice Enqueue new withdrawals via proof (proof operation)
    /// @dev Handles race condition where pending was swapped away during proving
    function enqueueWithProof(
        WithdrawalQueue storage q,
        WithdrawalQueueTransition memory transition
    ) internal;
}
```

| Queue | On-chain op | Proof op |
|-------|-------------|----------|
| Deposit | `enqueue` | `dequeueWithProof` |
| Withdrawal | `dequeue` | `enqueueWithProof` |

### Tempo contracts

#### Zone factory

```solidity
interface IZoneFactory {
    struct CreateZoneParams {
        address token;
        address sequencer;
        address verifier;
        bytes32 genesisStateRoot;
    }

    event ZoneCreated(
        uint64 indexed zoneId,
        address indexed portal,
        address indexed messenger,
        address token,
        address sequencer,
        address verifier,
        bytes32 genesisStateRoot
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
        uint64 indexed zoneId,
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed sender,
        address to,
        uint128 amount,
        bytes32 memo,
        bytes32 parentBlockHash,
        uint64 blockNumber,
        uint64 timestamp
    );

    event L1SyncAppended(
        uint64 indexed zoneId,
        bytes32 indexed newCurrentDepositQueueHash,
        bytes32 l1ParentBlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    event BatchSubmitted(
        uint64 indexed zoneId,
        uint64 indexed batchIndex,
        bytes32 prevSnapshotDepositQueueHash,     // pre-state input to verifier
        bytes32 prevProcessedDepositQueueHash,   // pre-state input to verifier
        bytes32 nextProcessedDepositQueueHash,   // verifier input/output
        bytes32 prevStateRoot,                   // pre-state input to verifier
        bytes32 nextStateRoot,                   // verifier output
        bytes32 prevPendingWithdrawalQueueHash,          // verifier input
        bytes32 nextPendingWithdrawalQueueHashIfFull,    // verifier output path 1
        bytes32 nextPendingWithdrawalQueueHashIfEmpty    // verifier output path 2
    );

    function zoneId() external view returns (uint64);
    function token() external view returns (address);
    function sequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
    function verifier() external view returns (address);
    function batchIndex() external view returns (uint64);
    function stateRoot() external view returns (bytes32);
    function processedDepositQueueHash() external view returns (bytes32);
    function snapshotDepositQueueHash() external view returns (bytes32);
    function currentDepositQueueHash() external view returns (bytes32);
    function activeWithdrawalQueueHash() external view returns (bytes32);
    function pendingWithdrawalQueueHash() external view returns (bytes32);

    /// @notice Set the sequencer's public key. Only callable by the sequencer.
    function setSequencerPubkey(bytes32 pubkey) external;

    /// @notice Deposit gas token into the zone. Returns the new current deposit queue hash.
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositQueueHash);

    /// @notice Append an L1 sync message to the deposit queue. Only callable by the sequencer.
    function syncL1() external returns (bytes32 newCurrentDepositQueueHash);

    /// @notice Process the next withdrawal from the active queue. Only callable by the sequencer.
    /// @param w The withdrawal to process (must be at the head of the active queue).
    /// @param remainingQueue The hash of the remaining queue after this withdrawal.
    function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external;

    /// @notice Submit a batch and verify the proof. Only callable by the sequencer.
    /// @param stateTransition The state root transition (prev filled from storage, next from batch).
    /// @param depositQueueTransition The deposit queue transition (prev hashes from storage).
    /// @param withdrawalQueueTransition The withdrawal queue transition with both possible outcomes.
    /// @param verifierData Opaque payload forwarded to verifier (e.g., attestation envelope).
    /// @param proof The validity proof or TEE attestation.
    function submitBatch(
        StateTransition calldata stateTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierData,
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

#### Zone registry (optional)

```solidity
interface IZoneRegistry {
    event ZoneRegistered(uint64 indexed zoneId, address indexed portal);
    event BatchHeadUpdated(uint64 indexed zoneId, uint64 indexed batchIndex, bytes32 stateRoot);

    function registerZone(ZoneInfo calldata info) external;
    function getZone(uint64 zoneId) external view returns (ZoneInfo memory);
    function batchHead(uint64 zoneId) external view returns (uint64 batchIndex, bytes32 stateRoot);
}
```

### Zone predeploys

#### Zone messenger (L2)

The L2 messenger is a predeploy that relays deposit callbacks. Deposit callbacks originate from this contract.

```solidity
// Predeploy address: 0x4200000000000000000000000000000000000007
interface IL2ZoneMessenger {
    /// @notice Returns the L1 sender during callback execution
    /// @dev Reverts if not in a callback context
    function xDomainMessageSender() external view returns (address);

    /// @notice Relay a deposit message. Only callable by system.
    /// @dev The gas token has already been credited to target before this call.
    /// @param sender The L1 origin address
    /// @param target The L2 recipient
    /// @param amount Amount that was credited
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

Receivers check `msg.sender == L2_MESSENGER` and call `messenger.xDomainMessageSender()` to authenticate the L1 origin.

#### Zone gas token

The zone's gas token is the bridged TIP-20 from Tempo. It is deployed at the **same address** on the zone as on Tempo. Users interact with it via the standard TIP-20 interface for transfers and approvals. The zone sequencer mints tokens when processing deposits and burns them when withdrawals are requested.

#### L1 state reader

The L1 state reader precompile allows zone contracts to read L1 storage. The zone node provides L1 state values; the prover validates all accesses against the state root committed in `parentBlockHash`. This is similar to [RIP-7728 (L1SLOAD)](https://github.com/ethereum/RIPs/pull/27).

```solidity
// Predeploy address: 0x000000000000000000000000000000000000c1000
interface IL1StateReader {
    /// @notice Read a storage slot from an L1 contract
    /// @param account The L1 contract address
    /// @param slot The storage slot to read
    /// @return value The storage value
    function readStorageSlot(address account, bytes32 slot) external view returns (bytes32);

    /// @notice Read multiple storage slots from an L1 contract
    /// @param account The L1 contract address
    /// @param slots The storage slots to read
    /// @return values The storage values
    function readStorageSlots(address account, bytes32[] calldata slots) external view returns (bytes32[] memory);
}
```

L1 state staleness depends on deposit frequency—the zone uses L1 state from the latest processed deposit's parent block. The prover includes Merkle proofs for each unique account and storage slot accessed during the batch.

#### TIP-403 registry

The zone has a `TIP403Registry` contract deployed at the **same address** as L1. This contract is read-only—it does not support writing policies. Its `isAuthorized` function reads policy state from L1 via the L1 state reader precompile, so zone-side TIP-20 transfers enforce L1 TIP-403 policies automatically.

#### Zone deposit queue

The zone deposit queue is a system contract that processes deposit queue messages from Tempo. It is called by the sequencer as a **system transaction at the start of each block** to apply deposits and L1 syncs.

```solidity
interface IZoneDepositQueue {
    event DepositProcessed(
        bytes32 indexed depositHash,
        address indexed sender,
        address indexed to,
        uint128 amount,
        bytes32 memo,
        bytes32 parentBlockHash,
        uint64 blockNumber,
        uint64 timestamp
    );

    event L1SyncProcessed(
        bytes32 indexed messageHash,
        bytes32 l1ParentBlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    /// @notice The last processed deposit queue hash (should match L1's processedDepositQueueHash after batch).
    function processedDepositQueueHash() external view returns (bytes32);

    /// @notice Latest L1 head observed from deposit queue messages.
    function l1ParentBlockHash() external view returns (bytes32);
    function l1BlockNumber() external view returns (uint64);
    function l1Timestamp() external view returns (uint64);

    /// @notice Process deposit queue messages from Tempo. Called by sequencer as system transaction.
    /// @dev Messages must be processed in order. The hash chain is verified.
    /// @param messages Array of deposit queue messages to process (oldest first).
    /// @param expectedHash The expected hash after processing all messages.
    function processDepositQueue(DepositQueueMessage[] calldata messages, bytes32 expectedHash) external;
}
```

The sequencer observes `DepositMade` and `L1SyncAppended` events on the L1 portal and relays them to the zone via `processDepositQueue`. Deposit messages credit the recipient's TIP-20 balance (minted by the system). L1 sync messages update the zone's L1 head without changing balances.

#### Zone outbox

The zone outbox handles withdrawal requests. Users approve the outbox to spend their gas tokens, then call `requestWithdrawal`. The outbox burns the tokens and emits an event. The sequencer collects these events to build the withdrawal queue for batch submission.

```solidity
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

    /// @notice The gas token address (same as L1 portal's token).
    function gasToken() external view returns (address);

    /// @notice Next withdrawal index (monotonically increasing).
    function nextWithdrawalIndex() external view returns (uint64);

    /// @notice Request a withdrawal from the zone back to Tempo.
    /// @dev Caller must have approved the outbox to spend `amount` of gas tokens.
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
}
```

The withdrawal queue hash is constructed off-chain by the sequencer from the emitted events:

```
// Build queue hash from events (oldest = outermost for O(1) L1 removal)
queueHash = 0
for withdrawal in withdrawals (newest to oldest):
    queueHash = keccak256(abi.encode(withdrawal, queueHash))
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

Both `depositQueue` messages and the withdrawal queue are FIFO queues that require constant on-chain storage. They have symmetric but inverted requirements:

|                      | Deposit queue messages | Withdrawal queue |
|----------------------|----------------|-------------|
| On-chain operation   | Add (users deposit / sequencer sync) | Remove (sequencer processes) |
| Proven operation     | Remove (zone consumes) | Add (zone creates) |
| Efficient on-chain   | Addition | Removal |
| Stable proving target| For removals | For additions |

Both use hash chains, but with different models:

- **Deposit queue messages**: 3 slots (`processedDepositQueueHash` is where proofs start, `snapshotDepositQueueHash` is the stable target, `currentDepositQueueHash` is the head where new messages land)
- **Withdrawal queue**: 2 separate queues (`activeWithdrawalQueueHash` drains, `pendingWithdrawalQueueHash` fills, then swap)

The hash chains are structured differently to optimize for their on-chain operation:

### Deposit queue: newest-outermost

```
Newest message wraps the outside (O(1) addition):

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

Adding d4: currentDepositQueueHash = keccak256(abi.encode(message4, currentDepositQueueHash))
```

- **On-chain addition is O(1)**: `currentDepositQueueHash = keccak256(abi.encode(message, currentDepositQueueHash))` — wrap the outside.
- **Proving removals**: Proof starts from stable `processedDepositQueueHash`, processes messages in FIFO order (oldest first, working outward), and must prove the result is an ancestor of `snapshotDepositQueueHash`.
- **After batch**: `processedDepositQueueHash = nextProcessedDepositQueueHash`, then `snapshotDepositQueueHash = currentDepositQueueHash` (snapshot new target for next proof).

### Withdrawal queue: oldest-outermost

```
Oldest withdrawal on the outside (O(1) removal):

                    ┌─────────────────────────────────────────┐
                    │ hash(w1, ┌─────────────────────────┐ ) │  ← activeWithdrawalQueueHash
                    │          │ hash(w2, ┌───────────┐ ) │  │
                    │          │          │ hash(w3,0) │   │  │
                    │          │          └───────────┘   │  │
                    │          └─────────────────────────┘  │
                    └─────────────────────────────────────────┘
                      ▲                              ▲
                      │                              │
                    oldest                        newest
                   (outermost)                  (innermost)

Removing w1: verify hash(w1, remainingQueue) == queue, then queue = remainingQueue
```

- **On-chain removal is O(1)**: Sequencer provides withdrawal + remaining hash, portal verifies and unwraps one layer.
- **Proving additions**: Proof builds queue with new withdrawals at innermost (O(N) inside ZKP).
- **Two queues handle the race**: `activeWithdrawalQueueHash` for processing, `pendingWithdrawalQueueHash` for accumulation. When `activeWithdrawalQueueHash` empties, swap in `pendingWithdrawalQueueHash`.

```
Two-queue system:

  ┌──────────────────┐         ┌──────────────────┐
  │  activeWithdrawalQueueHash │         │  pendingWithdrawalQueueHash │
  │  (being drained)  │         │  (being filled)   │
  └────────┬─────────┘         └────────┬─────────┘
           │                            │
           ▼                            ▼
      ┌─────────┐                 ┌─────────┐
      │ w1 ──────► process        │ w4      │ ◄── new from proof
      │ w2      │                 │ w5      │ ◄── new from proof
      │ w3      │                 │ w6      │ ◄── new from proof
      └─────────┘                 └─────────┘

When active is empty:
  activeWithdrawalQueueHash = pendingWithdrawalQueueHash
  pendingWithdrawalQueueHash = 0
```

The key insight: structure the hash chain so the **on-chain operation touches the outermost layer**. Additions wrap the outside; removals unwrap from the outside. The expensive operation (processing the full queue) happens inside the ZKP where O(N) is acceptable.

## Bridging in (Tempo to zone)

1. User calls `ZonePortal.deposit(to, amount, memo)` on Tempo.
2. `ZonePortal` transfers `amount` of the gas token into escrow and appends a deposit queue message: `currentDepositQueueHash = keccak256(abi.encode(message, currentDepositQueueHash))`. The deposit message includes current L1 block info (parent block hash, block number, timestamp) and the memo.
3. The sequencer observes `DepositMade` and `L1SyncAppended` events and processes messages in order via `processDepositQueue`, crediting `to` with `amount` of the gas token (TIP-20 balance) for deposit messages and updating the zone's L1 head for sync messages. Deposits always succeed—there is no callback or bounce mechanism.
4. A batch proof/attestation must prove the zone processed deposit queue messages from `processedDepositQueueHash` up to `nextProcessedDepositQueueHash`, and that `nextProcessedDepositQueueHash` is an ancestor of `snapshotDepositQueueHash`.
5. After the batch is accepted, `processedDepositQueueHash = nextProcessedDepositQueueHash` and `snapshotDepositQueueHash = currentDepositQueueHash` (snapshot for next proof).

Notes:

- Deposits are simple token credits. There are no callbacks or failure modes on the zone side.
- Deposits are finalized for Tempo once the batch is verified.
- There is no forced inclusion. If the sequencer withholds deposits, funds are stuck in escrow.
- The portal only stores three hashes, not individual messages. The sequencer must track messages off-chain.
- L1 block info is embedded in each deposit queue message, so the zone receives L1 state in-order with deposits and L1 syncs.
- The 3-slot design ensures on-chain verifiability: the proof cannot claim to process fake messages beyond `snapshotDepositQueueHash`.

## Bridging out (zone to Tempo)

Users withdraw by creating a withdrawal on the zone. Withdrawals are processed in two steps:

1. **Batch submission**: The proof adds new withdrawals to `pendingWithdrawalQueueHash`.
2. **Withdrawal processing**: The sequencer calls `processWithdrawal` to process withdrawals from `activeWithdrawalQueueHash` one at a time.

This separation allows the sequencer to process withdrawals immediately without waiting for proofs.

### Withdrawal execution

When the sequencer processes a withdrawal via `processWithdrawal`, the withdrawal is **popped unconditionally** (even on failure). If the messenger call fails, funds are bounced back via a new deposit.

```solidity
function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external onlySequencer {
    // Swap in pending if active is empty
    if (activeWithdrawalQueueHash == bytes32(0)) {
        require(pendingWithdrawalQueueHash != bytes32(0), "no withdrawals");
        activeWithdrawalQueueHash = pendingWithdrawalQueueHash;
        pendingWithdrawalQueueHash = bytes32(0);
    }

    // Verify head
    require(keccak256(abi.encode(w, remainingQueue)) == activeWithdrawalQueueHash, "invalid");

    // Pop the withdrawal regardless of success/failure
    if (remainingQueue == bytes32(0)) {
        activeWithdrawalQueueHash = pendingWithdrawalQueueHash;
        pendingWithdrawalQueueHash = bytes32(0);
    } else {
        activeWithdrawalQueueHash = remainingQueue;
    }

    if (w.gasLimit == 0) {
        ITIP20(token).transfer(w.to, w.amount);
        return;
    }

    // Messenger has max approval, does transferFrom + callback atomically
    try messenger.relayMessage(w.sender, w.to, w.amount, w.gasLimit, w.data) {
        // Success: messenger transferred tokens and executed callback
    } catch {
        // Callback failed: messenger reverted (including the transferFrom)
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
        parentBlockHash: blockhash(block.number - 1),
        blockNumber: uint64(block.number),
        timestamp: uint64(block.timestamp),
        sender: address(this),
        to: fallbackRecipient,
        amount: amount,
        memo: bytes32(0)
    });
    DepositQueueMessage memory m = DepositQueueMessage({
        kind: DepositQueueMessageKind.Deposit,
        data: abi.encode(d)
    });
    currentDepositQueueHash = keccak256(abi.encode(m, currentDepositQueueHash));
    emit DepositMade(...);
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

- **State root**: Computed via SP1 (Succinct) proving. The state root is the output of the proven execution.
- **L1 anchoring**: Each deposit queue message embeds L1 block info (parent block hash, block number, timestamp); off-chain observers reconstruct the exact L1 context from deposit queue events without a separate receipts root commitment. The parent block hash enables the L1 state reader precompile.

### Batching and proofs

- **Batch interval**: Batches are produced every 250 milliseconds.
- **SP1 proofs**: Validity proofs are generated using Succinct's SP1 prover.
- **Mock proofs**: For development, proofs are mocked but data structures (public inputs, proof envelope) must match the real format.
- **Sequencer posting only**: Only the configured sequencer posts batch proofs to the L1 portal. The proof includes state root and processed deposit queue messages.

```solidity
struct BatchProof {
    bytes32 nextStateRoot;
    bytes32 nextProcessedDepositQueueHash;
    bytes32 prevPendingWithdrawalQueueHash;
    bytes32 nextPendingWithdrawalQueueHashIfFull;
    bytes32 nextPendingWithdrawalQueueHashIfEmpty;
    bytes verifierData; // opaque payload to IVerifier (TEE/ZK envelope)
    bytes proof;        // SP1 proof bytes (or TEE attestation)
}
```
`prevStateRoot`, `processedDepositQueueHash`, and `snapshotDepositQueueHash` come from the portal's tracked state when the proof is verified on L1.

### Deposit queue messages and withdrawals

- **Deposit contract**: L1 portal escrows TIP-20 tokens. The ExEx watches `DepositMade` and `L1SyncAppended` events and queues deposit queue messages for zone processing.
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
