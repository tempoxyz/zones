# Tempo Zone Validium Design (Draft)

This document proposes a new validium protocol designed for Tempo. It is a design overview, not a full specification.

## Goals

- Create a Tempo-native validium called a zone.
- Each zone has exactly one permissioned sequencer.
- Each zone bridges exactly one TIP-20 token, which is also the zone gas token.
- Settlement uses fast validity proofs or TEE attestations (ZK or TEE). Data availability is fully trusted to the sequencer.
- Cross-chain operations are Tempo-centric: bridge in, bridge out (with optional callback to receiver contracts for composability).
- Verifier is abstracted behind a minimal `IVerifier` interface.

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

The sequencer posts batches to Tempo via a single `submitBatch` call that:

1. Verifies the proof/attestation for the state transition.
2. Updates the portal's state root.
3. Updates the withdrawal queue (adds new withdrawals to `withdrawalQueue2`).

Each batch submission includes:

- `newProcessedDepositsHash` (the deposits processed up to)
- `newStateRoot` (the resulting state after execution)
- `expectedQueue2` (the queue2 value the proof assumed)
- `updatedQueue2` (queue2 with new withdrawals if expectedQueue2 matches)
- `newWithdrawalsOnly` (new withdrawals only, if queue2 was swapped during proving)
- `proof` (validity proof or TEE attestation)

The portal tracks `stateRoot`, `checkpointedDepositsHash` (where proofs start from), `currentDepositsHash` (head of deposit chain), `withdrawalQueue1` (active queue), and `withdrawalQueue2` (pending queue).

The portal calls the verifier to validate the batch:

```solidity
interface IVerifier {
    function verify(
        // Deposit chain
        bytes32 checkpointedDepositsHash,  // where proof starts (from portal state)
        bytes32 newProcessedDepositsHash,  // where zone processed up to (from batch)

        // Zone state transition
        bytes32 prevStateRoot,
        bytes32 newStateRoot,

        // Withdrawal queue updates (proof outputs)
        bytes32 expectedQueue2,       // what proof assumed queue2 was
        bytes32 updatedQueue2,        // queue2 with new withdrawals added to innermost
        bytes32 newWithdrawalsOnly,   // new withdrawals only (only used if queue2 was empty)

        // Proof
        bytes calldata proof
    ) external view returns (bool);
}
```

The verifier validates that the state transition from `prevStateRoot` to `newStateRoot` is correct given the inputs.

### Deposit chain

Deposits use a hash chain: each deposit updates the hash as `newHash = keccak256(abi.encode(deposit, prevHash))`. The portal stores only the current chain head in a single storage slot. Each deposit includes L1 block info (hash, timestamp, block number), so the zone receives L1 state through the deposit chain rather than a separate anchor.

The proof must verify that the zone correctly processed deposits from `checkpointedDepositsHash` to `newProcessedDepositsHash`.

After the batch is accepted, the portal updates `checkpointedDepositsHash = newProcessedDepositsHash`. This means the next proof will start from where this proof ended.

Proofs or attestations are assumed to be fast. No data availability is required by the verifier.

## Withdrawal queue

Withdrawals use a two-queue system that allows the sequencer to process withdrawals independently of proof generation. The portal tracks two hash chains in constant space (2 storage slots):

- `withdrawalQueue1` - the active queue, processed by the sequencer
- `withdrawalQueue2` - the pending queue, updated by proofs

### Hash chain structure

Each queue is a hash chain with the **oldest withdrawal at the outermost layer**, making FIFO processing efficient:

```
queue = keccak256(abi.encode(w1, keccak256(abi.encode(w2, keccak256(abi.encode(w3, bytes32(0)))))))
        // w1 is oldest (outermost), w3 is newest (innermost)
```

To process the oldest withdrawal, the sequencer provides the withdrawal data and the remaining queue hash. The portal verifies the hash and advances the queue:

```solidity
function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external onlySequencer {
    // If queue1 is empty, swap in queue2 first
    if (withdrawalQueue1 == bytes32(0)) {
        require(withdrawalQueue2 != bytes32(0), "no withdrawals");
        withdrawalQueue1 = withdrawalQueue2;
        withdrawalQueue2 = bytes32(0);
    }

    require(keccak256(abi.encode(w, remainingQueue)) == withdrawalQueue1, "invalid");

    _executeWithdrawal(w);

    if (remainingQueue == bytes32(0)) {
        // Queue1 exhausted, swap in queue2
        withdrawalQueue1 = withdrawalQueue2;
        withdrawalQueue2 = bytes32(0);
    } else {
        withdrawalQueue1 = remainingQueue;
    }
}
```

### Proof updates to queue2

When a proof is submitted, it adds new withdrawals to `withdrawalQueue2`. The proof builds the queue with new withdrawals at the **innermost** layers (newest = last to process). This is an O(N) operation but happens inside the ZKP.

### Race condition handling

A race condition can occur:
1. Proof generation starts when `withdrawalQueue2 = X` (non-empty)
2. Meanwhile, sequencer drains `withdrawalQueue1`, triggering swap: `withdrawalQueue1 = X`, `withdrawalQueue2 = 0`
3. Proof submits expecting `withdrawalQueue2 = X`, but it's now `0`

To handle this, the proof generates two outputs:
- `updatedQueue2` - new withdrawals added to innermost of the expected queue2
- `newWithdrawalsOnly` - new withdrawals as a fresh queue (as if queue2 was empty)

The caller provides `expectedQueue2` (what the proof assumed), and the portal uses the appropriate value:

```solidity
function submitBatch(
    ...,
    bytes32 expectedQueue2,
    bytes32 updatedQueue2,
    bytes32 newWithdrawalsOnly
) external onlySequencer {
    // ... verify proof ...

    if (withdrawalQueue2 == expectedQueue2) {
        withdrawalQueue2 = updatedQueue2;
    } else if (withdrawalQueue2 == bytes32(0)) {
        withdrawalQueue2 = newWithdrawalsOnly;
    } else {
        revert("unexpected queue2 state");
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
    address token;
    address sequencer;
    address verifier;
    bytes32 genesisStateRoot;
}

struct BatchCommitment {
    bytes32 newProcessedDepositsHash;
    bytes32 newStateRoot;
}

struct Deposit {
    // L1 block info (zone receives L1 state through deposits)
    bytes32 l1BlockHash;
    uint64 l1BlockNumber;
    uint64 l1Timestamp;
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
    uint64 gasLimit;            // max gas for IExitReceiver callback (0 = no callback)
    address fallbackRecipient;  // zone address for bounce-back if call fails
    bytes data;                 // calldata for IExitReceiver (if gasLimit > 0)
}
```

### Verifier

```solidity
interface IVerifier {
    function verify(
        // Deposit chain
        bytes32 checkpointedDepositsHash,  // where proof starts (from portal state)
        bytes32 newProcessedDepositsHash,  // where zone processed up to (from batch)

        // Zone state transition
        bytes32 prevStateRoot,
        bytes32 newStateRoot,

        // Withdrawal queue updates (proof outputs)
        bytes32 expectedQueue2,       // what proof assumed queue2 was
        bytes32 updatedQueue2,        // queue2 with new withdrawals added to innermost
        bytes32 newWithdrawalsOnly,   // new withdrawals only (only used if queue2 was empty)

        // Proof
        bytes calldata proof
    ) external view returns (bool);
}
```

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
        address indexed token,
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
    event Deposit(
        uint64 indexed zoneId,
        bytes32 indexed newCurrentDepositsHash,
        address indexed sender,
        address to,
        uint128 amount,
        bytes32 memo,
        bytes32 l1BlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    event BatchSubmitted(
        uint64 indexed zoneId,
        uint64 indexed batchIndex,
        bytes32 newProcessedDepositsHash,
        bytes32 newStateRoot
    );

    function zoneId() external view returns (uint64);
    function token() external view returns (address);
    function sequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
    function verifier() external view returns (address);
    function batchIndex() external view returns (uint64);
    function stateRoot() external view returns (bytes32);
    function currentDepositsHash() external view returns (bytes32);
    function checkpointedDepositsHash() external view returns (bytes32);
    function withdrawalQueue1() external view returns (bytes32);
    function withdrawalQueue2() external view returns (bytes32);

    /// @notice Set the sequencer's public key. Only callable by the sequencer.
    function setSequencerPubkey(bytes32 pubkey) external;

    /// @notice Deposit gas token into the zone. Returns the new current deposits hash.
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositsHash);

    /// @notice Process the next withdrawal from queue1. Only callable by the sequencer.
    /// @param w The withdrawal to process (must be at the head of queue1).
    /// @param remainingQueue The hash of the remaining queue after this withdrawal.
    function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external;

    /// @notice Submit a batch and verify the proof. Only callable by the sequencer.
    /// @param commitment The batch commitment (new state root and processed deposits hash).
    /// @param expectedQueue2 The queue2 value the proof assumed during generation.
    /// @param updatedQueue2 New queue2 if expectedQueue2 matches current queue2.
    /// @param newWithdrawalsOnly New queue2 if current queue2 is empty (swap occurred).
    /// @param proof The validity proof or TEE attestation.
    function submitBatch(
        BatchCommitment calldata commitment,
        bytes32 expectedQueue2,
        bytes32 updatedQueue2,
        bytes32 newWithdrawalsOnly,
        bytes calldata proof
    ) external;
}
```

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

#### Zone outbox

```solidity
interface IZoneOutbox {
    event WithdrawalRequested(bytes32 indexed withdrawalId, uint64 indexed withdrawalIndex);

    function nextWithdrawalIndex() external view returns (uint64);
    function withdrawalByIndex(uint64 index) external view returns (Withdrawal memory);

    function requestWithdrawal(
        address to,
        uint128 amount,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes calldata data
    ) external returns (bytes32 withdrawalId);
}
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

#### Exit receiver

Contracts that want to receive transfer exits with a callback must implement this interface:

```solidity
interface IExitReceiver {
    /// @notice Called by the portal when a transfer exit targets this contract.
    /// @param sender The address that initiated the exit on the zone.
    /// @param amount The amount of gas tokens transferred.
    /// @param data User-provided data from the exit intent.
    /// @return selector Must return IExitReceiver.onExitReceived.selector to accept.
    function onExitReceived(
        address sender,
        uint128 amount,
        bytes calldata data
    ) external returns (bytes4);
}
```

## Queue design rationale

Both deposits and withdrawals are FIFO queues that require constant on-chain storage. They have symmetric but inverted requirements:

|                      | Deposits | Withdrawals |
|----------------------|----------|-------------|
| On-chain operation   | Add (users deposit) | Remove (sequencer processes) |
| Proven operation     | Remove (zone consumes) | Add (zone creates) |
| Efficient on-chain   | Addition | Removal |
| Stable proving target| For removals | For additions |

Both use hash chains with 2 storage slots, but with different models:

- **Deposits**: 1 queue + cursor (`currentDepositsHash` is the head, `checkpointedDepositsHash` is a cursor into the queue)
- **Withdrawals**: 2 separate queues (`withdrawalQueue1` drains, `withdrawalQueue2` fills, then swap)

The hash chains are structured differently to optimize for their on-chain operation:

### Deposit queue: newest-outermost

![Deposit Queue](docs/diagrams/deposit-queue.svg)

- **On-chain addition is O(1)**: `deposits = keccak256(abi.encode(deposit, deposits))` — wrap the outside.
- **Proving removals**: Proof starts from stable `checkpointedDepositsHash`, processes deposits in FIFO order (oldest first, working outward from the checkpoint).
- **Checkpoint advances after batch**: `checkpointedDepositsHash = newProcessedDepositsHash`

### Withdrawal queue: oldest-outermost

![Withdrawal Queue](docs/diagrams/withdrawal-queue.svg)

- **On-chain removal is O(1)**: Sequencer provides withdrawal + remaining hash, portal verifies and unwraps one layer.
- **Proving additions**: Proof builds queue with new withdrawals at innermost (O(N) inside ZKP).
- **Two queues handle the race**: `queue1` for processing, `queue2` for accumulation. When `queue1` empties, swap in `queue2`.

![Two-Queue Swap](docs/diagrams/two-queue-swap.svg)

The key insight: structure the hash chain so the **on-chain operation touches the outermost layer**. Additions wrap the outside; removals unwrap from the outside. The expensive operation (processing the full queue) happens inside the ZKP where O(N) is acceptable.

## Bridging in (Tempo to zone)

1. User calls `ZonePortal.deposit(to, amount, memo)` on Tempo.
2. `ZonePortal` transfers `amount` of the gas token into escrow and updates the deposits hash chain: `currentDepositsHash = keccak256(abi.encode(deposit, currentDepositsHash))`. The deposit includes current L1 block info (hash, number, timestamp).
3. The sequencer observes deposit events, processes them in order, and credits the zone recipient. The zone receives L1 state through the deposit data.
4. A batch proof/attestation must prove the zone processed deposits up to `newProcessedDepositsHash`.

Notes:

- Deposits are finalized for Tempo once the batch is verified.
- There is no forced inclusion. If the sequencer withholds deposits, funds are stuck in escrow.
- The portal only stores the chain head hash, not individual deposits. The sequencer must track deposits off-chain.
- L1 block info is embedded in each deposit, so the zone receives L1 state through the deposit chain.

## Bridging out (zone to Tempo)

Users withdraw by creating a withdrawal on the zone. Withdrawals are processed in two steps:

1. **Batch submission**: The proof adds new withdrawals to `withdrawalQueue2`.
2. **Withdrawal processing**: The sequencer calls `processWithdrawal` to process withdrawals from `withdrawalQueue1` one at a time.

This separation allows the sequencer to process withdrawals immediately without waiting for proofs.

### Withdrawal execution

When the sequencer processes a withdrawal via `processWithdrawal`:

1. Transfer tokens to the `to` address.
2. If `gasLimit > 0`, call `IExitReceiver.onExitReceived` on the recipient with the specified gas limit.
3. If the call fails or returns the wrong selector, bounce back the funds to the zone.

```solidity
function _executeWithdrawal(Withdrawal calldata w) internal {
    // Transfer tokens first
    ITIP20(token).transfer(w.to, w.amount);

    // If callback requested, call the receiver with gas limit
    if (w.gasLimit > 0) {
        try IExitReceiver(w.to).onExitReceived{gas: w.gasLimit}(
            w.sender,
            w.amount,
            w.data
        ) returns (bytes4 selector) {
            require(selector == IExitReceiver.onExitReceived.selector, "rejected");
        } catch {
            // Call failed or rejected: bounce back to zone
            _enqueueBounceBack(w.amount, w.fallbackRecipient);
        }
    }
}
```

This enables composable withdrawals where funds can flow directly into Tempo contracts (e.g., DEX swaps, staking, cross-zone deposits). The portal does not need to know about these use cases—they are handled by receiver contracts.

## Withdrawal failure and bounce-back

Withdrawals with `gasLimit > 0` can fail if the `IExitReceiver` call fails, runs out of gas, or returns the wrong selector. When this happens, the portal "bounces back" the funds by re-depositing into the same zone to the withdrawal's `fallbackRecipient`.

```solidity
function _enqueueBounceBack(uint128 amount, address fallbackRecipient) internal {
    currentDepositsHash = keccak256(abi.encode(currentDepositsHash, Deposit({
        sender: address(this),
        to: fallbackRecipient,
        amount: amount,
        memo: bytes32(0),
        ...
    })));
    emit Deposit(...);
}
```

The zone processes bounce-back deposits and credits the `fallbackRecipient`. This allows withdrawals to fail gracefully without blocking the queue.

## Data availability and liveness

- Zone data availability is fully trusted to the sequencer.
- If the sequencer withholds data or halts, users cannot reconstruct zone state or force exits.
- The design assumes users accept this risk in exchange for low-cost and fast settlement.

## Security considerations

- Sequencer can halt the zone without recourse due to missing data availability.
- The verifier is a trust anchor. A faulty verifier can steal or lock funds.
- Withdrawals with callbacks only call the `IExitReceiver` interface with a user-specified gas limit—no arbitrary calldata or unbounded gas. Receivers must return the correct selector to accept funds; failed or rejected calls trigger a bounce-back to `fallbackRecipient`.
- Deposits are locked on Tempo until a verified batch consumes them.

## Open questions

- Should deposits be cancellable if not consumed within a timeout?
