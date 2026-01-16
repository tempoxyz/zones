# Tempo Zone Validium Design (Draft)

This document proposes a new validium protocol designed for Tempo. It is a design overview, not a full specification.

## Goals

- Create a Tempo-native validium called a zone.
- Each zone has exactly one permissioned sequencer.
- Each zone bridges exactly one TIP-20 token, which is also the zone gas token.
- Settlement uses fast validity proofs or TEE attestations (ZK or TEE). Data availability is fully trusted to the sequencer.
- Cross-chain operations are Tempo-centric: bridge in, bridge out, bridge out then swap on the Tempo DEX, and bridge out then swap then deposit into another zone.
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
3. Processes all exits from that batch (transfers, swaps, cross-zone deposits).

Each batch submission includes:

- `newDepositsHash` (the deposits processed up to)
- `newStateRoot` (the resulting state after execution)
- `exits` (full list of exit intents to process)
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

        // Exits
        bytes32 exitsHash,

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
enum ExitKind {
    Transfer,
    Swap,
    SwapAndDeposit
}

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

struct TransferExit {
    address recipient;
    uint128 amount;
}

struct SwapExit {
    address recipient;
    address tokenOut;
    uint128 amountIn;
    uint128 minAmountOut;
}

struct SwapAndDepositExit {
    address tokenOut;
    uint128 amountIn;
    uint128 minAmountOut;
    uint64 destinationZoneId;
    address destinationRecipient;
}

struct ExitIntent {
    ExitKind kind;
    address sender;
    TransferExit transfer;
    SwapExit swap;
    SwapAndDepositExit swapAndDeposit;
}
```

Exit intent fields are only meaningful for their `kind`. For example, `TransferExit` is used only when `kind == ExitKind.Transfer`.

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

        // Exits
        bytes32 exitsHash,

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

    /// @notice Submit a batch, verify the proof, and process all exits atomically.
    /// @dev Only callable by the sequencer.
    function submitBatch(
        BatchCommitment calldata commitment,
        ExitIntent[] calldata exits,
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
    event ExitRequested(bytes32 indexed exitId, uint64 indexed exitIndex, ExitKind kind);

    function nextExitIndex() external view returns (uint64);
    function exitByIndex(uint64 exitIndex) external view returns (ExitIntent memory);

    function requestTransferExit(
        address recipient,
        uint128 amount,
        bytes32 memo
    ) external returns (bytes32 exitId);

    function requestSwapExit(
        address tokenOut,
        uint128 amountIn,
        uint128 minAmountOut,
        address recipient,
        bytes32 memo
    ) external returns (bytes32 exitId);

    function requestSwapAndDepositExit(
        address tokenOut,
        uint128 amountIn,
        uint128 minAmountOut,
        uint64 destinationZoneId,
        address destinationRecipient,
        bytes32 memo
    ) external returns (bytes32 exitId);
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

#### Tempo Stablecoin DEX (minimal)

```solidity
interface IStablecoinDEX {
    function swapExactAmountIn(
        address tokenIn,
        address tokenOut,
        uint128 amountIn,
        uint128 minAmountOut
    ) external returns (uint128 amountOut);

    function swapExactAmountOut(
        address tokenIn,
        address tokenOut,
        uint128 amountOut,
        uint128 maxAmountIn
    ) external returns (uint128 amountIn);
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

Users exit by creating an exit intent on the zone. When the sequencer submits the batch containing that exit, the exit is processed immediately in the same transaction.

Exit intent types:

- Transfer: release the gas token to a Tempo recipient.
- Swap: release the gas token to the portal, swap on the Tempo DEX, and send output to a recipient.
- Swap and deposit: release the gas token, swap on the Tempo DEX, then deposit the output token into another zone.

There is no separate finalization step—users receive their funds as soon as the batch is submitted and verified.

## Tempo DEX integration

The portal interacts with the Tempo Stablecoin DEX at `0xdec0000000000000000000000000000000000000`. Swaps use the DEX functions:

- `swapExactAmountIn(tokenIn, tokenOut, amountIn, minAmountOut)`
- `swapExactAmountOut(tokenIn, tokenOut, amountOut, maxAmountIn)`

For exit intents that require a swap:

1. The portal approves the DEX to pull the `tokenIn` amount.
2. The portal executes the swap with slippage limits from the exit intent.
3. The output token is transferred to the recipient or used for a deposit into another zone.

For swap exits, `tokenIn` is always the zone gas token, and `tokenOut` is any Tempo TIP-20 token supported by the DEX routing rules.

## Zone-to-zone flow

Zone-to-zone transfer is a composition of exit and deposit, all processed atomically when the batch is submitted:

1. Zone A user submits an exit intent of type Swap and deposit.
2. When the zone A sequencer submits the batch, the portal:
   - Verifies the proof.
   - Swaps the gas token of zone A into the gas token of zone B on the Tempo DEX.
   - Calls `ZonePortal.deposit` for zone B with the swap output.
3. The zone B sequencer consumes the deposit and credits the recipient on zone B.

This flow uses Tempo as the hub and never requires direct zone-to-zone messaging.

## Data availability and liveness

- Zone data availability is fully trusted to the sequencer.
- If the sequencer withholds data or halts, users cannot reconstruct zone state or force exits.
- The design assumes users accept this risk in exchange for low-cost and fast settlement.

## Security considerations

- Sequencer can halt the zone without recourse due to missing data availability.
- The verifier is a trust anchor. A faulty verifier can steal or lock funds.
- Swap exits are exposed to DEX liquidity and slippage constraints. The exit intent must include limits to avoid adverse execution.
- Deposits are locked on Tempo until a verified batch consumes them.

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

### State and receipts

- **State root**: Computed via SP1 (Succinct) proving. The state root is the output of the proven execution.
- **Receipts trie**: Zone maintains a receipts trie for all executed transactions. The receipts root is included in batch commitments.

### Batching and proofs

- **Batch interval**: Batches are produced every 250 milliseconds.
- **SP1 proofs**: Validity proofs are generated using Succinct's SP1 prover.
- **Mock proofs**: For development, proofs are mocked but data structures (public inputs, proof envelope) must match the real format.
- **Permissionless posting**: Anyone can post a batch proof with headers to the L1 portal. The proof includes state root, receipts root, and processed deposits.

```solidity
struct BatchProof {
    bytes32 prevStateRoot;
    bytes32 newStateRoot;
    bytes32 receiptsRoot;
    bytes32 processedDepositsHash;
    bytes32 newDepositsHash;
    bytes proof;  // SP1 proof bytes (or mock)
}
```

### Deposits and withdrawals

- **Deposit contract**: L1 deposit contract escrows TIP-20 tokens. The ExEx watches deposit events and queues them for zone processing.
- **Withdrawal requests**: Users trigger withdrawals on L2 via RPC. The withdrawal is added to the pending exits and included in the next batch's exit list.

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
│   ├── Receipts Trie
│   └── SP1 Prover (mock for dev)
│
└── ExEx: USDT Zone
    ├── TIP-20 Precompile (USDT)
    ├── Payments Precompile
    ├── In-memory State Store
    ├── Receipts Trie
    └── SP1 Prover (mock for dev)
```

## Open questions

- Should deposits be cancellable if not consumed within a timeout?
- Should exit intents allow fee payment for DEX swaps in the output token?
- What is the state persistence strategy for zone recovery after node restart?
- Should batch posting be permissioned initially and opened later?
