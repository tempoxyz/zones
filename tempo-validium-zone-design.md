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

- `gasToken`: the Tempo TIP-20 address to bridge. This is the only bridged token and the gas token.
- `sequencer`: permissioned sequencer address.
- `verifier`: `IVerifier` implementation for proof or attestation.
- `zoneParams`: initial configuration (genesis state root, fee parameters).

The factory deploys a `ZonePortal` that escrows the gas token on Tempo. The zone genesis includes the portal address and the gas token configuration.

## Execution and fees

- The zone reuses Tempo's fee units and accounting model.
- The fee token is always the gas token. There is no fee token selection.
- Transactions use Tempo transaction semantics for fee payer, max fee per gas, and gas limit. The fee token field is fixed to the gas token.

## Batch submission

The sequencer posts batches to Tempo via a single `submitBatch` call that atomically:

1. Verifies the proof/attestation for the state transition.
2. Updates the portal's state root.
3. Processes all withdrawals from that batch.

Each batch submission includes:

- `newDepositsHash` (the deposits processed up to)
- `newStateRoot` (the resulting state after execution)
- `withdrawals` (full list of withdrawals to process)
- `proof` (validity proof or TEE attestation)

The portal tracks `stateRoot`, `processedDepositsHash` (what the zone has consumed), and `depositsHash` (all queued deposits).

The portal calls the verifier to validate the batch:

```solidity
interface IVerifier {
    function verify(
        // Deposit chain
        bytes32 processedDepositsHash,  // start (from portal state)
        bytes32 newDepositsHash,        // end (from commitment)
        bytes32 currentDepositsHash,    // portal's current head

        // Zone state transition
        bytes32 prevStateRoot,
        bytes32 newStateRoot,

        // Withdrawals
        bytes32 withdrawalsHash,

        // Proof
        bytes calldata proof
    ) external view returns (bool);
}
```

The verifier validates that the state transition from `prevStateRoot` to `newStateRoot` is correct given the inputs.

Deposits use a Merkle chain: each deposit updates the hash as `newHash = keccak256(prevHash, deposit)`. The portal stores only the current chain head in a single storage slot. Each deposit includes L1 block info (hash, timestamp, block number), so the zone receives L1 state through the deposit chain rather than a separate anchor.

The proof must verify:
- The zone correctly processed deposits from `processedDepositsHash` to `newDepositsHash`
- `newDepositsHash` is an ancestor of `currentDepositsHash` (a valid point in the chain)

After the batch is accepted, the portal updates `processedDepositsHash = newDepositsHash`.

Proofs or attestations are assumed to be fast. No data availability is required by the verifier.

This atomic design means users receive their exits immediately when the batch is posted—there is no separate finalization step.

## Interfaces and functions

This section defines the functions and interfaces used by the design. The signatures are Solidity-style and focus on the minimum surface area.

### Common types

```solidity

struct ZoneInfo {
    uint64 zoneId;
    address portal;
    address gasToken;
    address sequencer;
    address verifier;
    bytes32 genesisStateRoot;
}

struct BatchCommitment {
    bytes32 newDepositsHash;
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
    uint256 amount;
    bytes32 memo;
}

struct Withdrawal {
    address sender;             // who initiated the withdrawal on the zone
    address to;                 // Tempo recipient
    uint128 amount;
    address fallbackRecipient;  // zone address for bounce-back if call fails
    bytes data;                 // if non-empty, call IExitReceiver on `to`
}
```

### Verifier

```solidity
interface IVerifier {
    function verify(
        // Deposit chain
        bytes32 processedDepositsHash,  // start (from portal state)
        bytes32 newDepositsHash,        // end (from commitment)
        bytes32 currentDepositsHash,    // portal's current head

        // Zone state transition
        bytes32 prevStateRoot,
        bytes32 newStateRoot,

        // Withdrawals
        bytes32 withdrawalsHash,

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
        address gasToken;
        address sequencer;
        address verifier;
        bytes32 genesisStateRoot;
    }

    event ZoneCreated(
        uint64 indexed zoneId,
        address indexed portal,
        address indexed gasToken,
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
    event DepositEnqueued(
        uint64 indexed zoneId,
        bytes32 indexed newDepositsHash,
        address indexed sender,
        address to,
        uint256 amount,
        bytes32 memo,
        bytes32 l1BlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    event BatchSubmitted(
        uint64 indexed zoneId,
        uint64 indexed batchIndex,
        bytes32 newDepositsHash,
        bytes32 newStateRoot
    );

    function zoneId() external view returns (uint64);
    function gasToken() external view returns (address);
    function sequencer() external view returns (address);
    function sequencerPubkey() external view returns (bytes32);
    function verifier() external view returns (address);
    function batchIndex() external view returns (uint64);
    function stateRoot() external view returns (bytes32);
    function depositsHash() external view returns (bytes32);
    function processedDepositsHash() external view returns (bytes32);

    /// @notice Set the sequencer's public key. Only callable by the sequencer.
    function setSequencerPubkey(bytes32 pubkey) external;

    /// @notice Deposit gas token into the zone. Returns the new deposits chain hash.
    function deposit(address to, uint256 amount, bytes32 memo) external returns (bytes32 newDepositsHash);

    /// @notice Submit a batch, verify the proof, and process all withdrawals atomically.
    /// @dev Only callable by the sequencer.
    function submitBatch(
        BatchCommitment calldata commitment,
        Withdrawal[] calldata withdrawals,
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

## Bridging in (Tempo to zone)

1. User calls `ZonePortal.deposit(to, amount, memo)` on Tempo.
2. `ZonePortal` transfers `amount` of the gas token into escrow and updates the deposits hash chain: `depositsHash = keccak256(depositsHash, deposit)`. The deposit includes current L1 block info (hash, number, timestamp).
3. The sequencer observes deposit events, processes them in order, and credits the zone recipient. The zone receives L1 state through the deposit data.
4. A batch proof/attestation must prove the zone processed deposits up to `newDepositsHash`.

Notes:

- Deposits are finalized for Tempo once the batch is verified.
- There is no forced inclusion. If the sequencer withholds deposits, funds are stuck in escrow.
- The portal only stores the chain head hash, not individual deposits. The sequencer must track deposits off-chain.
- L1 block info is embedded in each deposit, so the zone receives L1 state through the deposit chain.

## Bridging out (zone to Tempo)

Users withdraw by creating a withdrawal on the zone. When the sequencer submits the batch containing that withdrawal, it is processed immediately in the same transaction.

The portal processes each withdrawal as follows:

1. Transfer tokens to the `to` address.
2. If `data` is non-empty, call `IExitReceiver.onExitReceived` on the recipient.
3. If the call fails or returns the wrong selector, bounce back the funds to the zone.

```solidity
// Transfer tokens first
ITIP20(gasToken).transfer(withdrawal.to, withdrawal.amount);

// If data provided, call the receiver
if (withdrawal.data.length > 0) {
    try IExitReceiver(withdrawal.to).onExitReceived(
        withdrawal.sender,
        withdrawal.amount,
        withdrawal.data
    ) returns (bytes4 selector) {
        require(selector == IExitReceiver.onExitReceived.selector, "rejected");
    } catch {
        // Call failed or rejected: bounce back to zone
        _enqueueBounceBack(withdrawal.amount, withdrawal.fallbackRecipient, withdrawalIndex);
    }
}
```

There is no separate finalization step—users receive their funds as soon as the batch is submitted and verified.

This enables composable withdrawals where funds can flow directly into Tempo contracts (e.g., DEX swaps, staking, cross-zone deposits) in a single atomic operation. The portal does not need to know about these use cases—they are handled by receiver contracts.

## Withdrawal failure and bounce-back

Withdrawals with `data` can fail if the `IExitReceiver` call fails or returns the wrong selector. When this happens, the portal does not revert the batch. Instead, it "bounces back" the funds by re-depositing into the same zone to the withdrawal's `fallbackRecipient`.

The bounce-back deposit uses the withdrawal index as the memo, allowing the zone to correlate the deposit with the original failed withdrawal:

```solidity
function _enqueueBounceBack(uint128 amount, address fallbackRecipient, uint64 withdrawalIndex) internal {
    bytes32 memo = bytes32(uint256(withdrawalIndex));
    depositsHash = keccak256(abi.encode(depositsHash, Deposit({
        sender: address(this),
        to: fallbackRecipient,
        amount: amount,
        memo: memo,
        ...
    })));
    emit DepositEnqueued(...);
}
```

The zone processes bounce-back deposits and credits the `fallbackRecipient`. This keeps batch submission atomic while allowing individual withdrawals to fail gracefully without blocking others.

## Data availability and liveness

- Zone data availability is fully trusted to the sequencer.
- If the sequencer withholds data or halts, users cannot reconstruct zone state or force exits.
- The design assumes users accept this risk in exchange for low-cost and fast settlement.

## Security considerations

- Sequencer can halt the zone without recourse due to missing data availability.
- The verifier is a trust anchor. A faulty verifier can steal or lock funds.
- Withdrawals with callbacks only call the `IExitReceiver` interface—no arbitrary calldata. Receivers must return the correct selector to accept funds; failed or rejected calls trigger a bounce-back to `fallbackRecipient`.
- Deposits are locked on Tempo until a verified batch consumes them.

## Open questions

- Should deposits be cancellable if not consumed within a timeout?
