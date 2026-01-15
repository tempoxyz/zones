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
- Batch: a sequencer-produced commitment to a zone state transition.

## System overview

### Actors

- Zone sequencer: permissioned operator that orders zone transactions, provides data, and posts batches to Tempo.
- Verifier: ZK proof system or TEE attester. Abstracted via `IVerifier`.
- Users: deposit TIP-20 from Tempo to the zone or exit back to Tempo.
- Relayers (optional): submit proofs or attestations to Tempo.

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

## Batch commitment and verification

The sequencer posts batches to Tempo. Each batch commitment includes:

- `zoneId`
- `prevStateRoot`, `newStateRoot`
- `depositIndex` (number of Tempo-side deposits consumed)
- `exitRoot` (commitment to zone exit intents)
- `batchHash` (opaque commitment to the executed transactions)

The portal calls the verifier to accept the batch:

```solidity
interface IVerifier {
    function verify(bytes32 batchCommitment, bytes calldata proof) external view returns (bool);
}
```

The verifier is expected to validate the state transition and the exit root. Proofs or attestations are assumed to be fast. No data availability is required by the verifier.

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
    uint64 zoneId;
    uint64 batchIndex;
    bytes32 prevStateRoot;
    bytes32 newStateRoot;
    uint64 depositIndex;
    bytes32 exitRoot;
    bytes32 batchHash;
}

struct Deposit {
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
    uint64 exitIndex;
    address sender;
    TransferExit transfer;
    SwapExit swap;
    SwapAndDepositExit swapAndDeposit;
    bytes32 memo;
}

struct ExitProof {
    uint64 batchIndex;
    bytes32[] merkleProof;
}
```

Exit intent fields are only meaningful for their `kind`. For example, `TransferExit` is used only when `kind == ExitKind.Transfer`. The `exitId` is `keccak256(abi.encode(zoneId, exitIndex, intent))`, and the portal rejects duplicate `exitId` values.

### Verifier

```solidity
interface IVerifier {
    function verify(bytes32 batchCommitment, bytes calldata proof) external view returns (bool);
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
        uint64 indexed depositIndex,
        address indexed sender,
        address to,
        uint256 amount,
        bytes32 memo
    );

    event BatchAccepted(
        uint64 indexed zoneId,
        uint64 indexed batchIndex,
        bytes32 prevStateRoot,
        bytes32 newStateRoot,
        uint64 depositIndex,
        bytes32 exitRoot,
        bytes32 batchHash
    );

    event ExitFinalized(bytes32 indexed exitId, uint64 indexed zoneId, ExitKind kind);

    function zoneId() external view returns (uint64);
    function gasToken() external view returns (address);
    function sequencer() external view returns (address);
    function verifier() external view returns (address);

    function nextDepositIndex() external view returns (uint64);
    function deposits(uint64 index) external view returns (Deposit memory);

    function deposit(address to, uint256 amount, bytes32 memo) external returns (uint64 depositIndex);

    function submitBatch(BatchCommitment calldata commitment, bytes calldata proof) external;

    function exitClaimed(bytes32 exitId) external view returns (bool);

    function finalizeTransferExit(
        ExitIntent calldata intent,
        ExitProof calldata proof
    ) external;

    function finalizeSwapExit(
        ExitIntent calldata intent,
        ExitProof calldata proof
    ) external;

    function finalizeSwapAndDepositExit(
        ExitIntent calldata intent,
        ExitProof calldata proof
    ) external;
}
```

#### Zone registry (optional)

```solidity
interface IZoneRegistry {
    event ZoneRegistered(uint64 indexed zoneId, address indexed portal);
    event BatchHeadUpdated(uint64 indexed zoneId, uint64 indexed batchIndex, bytes32 stateRoot, bytes32 exitRoot);

    function registerZone(ZoneInfo calldata info) external;
    function getZone(uint64 zoneId) external view returns (ZoneInfo memory);
    function batchHead(uint64 zoneId) external view returns (uint64 batchIndex, bytes32 stateRoot, bytes32 exitRoot);
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
2. `ZonePortal` transfers `amount` of the gas token into escrow and enqueues a deposit with a monotonically increasing index.
3. The sequencer consumes deposits in order and credits the zone recipient.
4. A batch proof/attestation must include the new `depositIndex`.

Notes:

- Deposits are finalized for Tempo once the batch is verified.
- There is no forced inclusion. If the sequencer withholds deposits, funds are stuck in escrow.

## Bridging out (zone to Tempo)

Users exit by creating an exit intent on the zone. Exit intents are committed to `exitRoot` in the batch commitment.

Exit intent types:

- Transfer: release the gas token to a Tempo recipient.
- Swap: release the gas token to the portal, swap on the Tempo DEX, and send output to a recipient.
- Swap and deposit: release the gas token, swap on the Tempo DEX, then deposit the output token into another zone.

Once a batch is verified, anyone can finalize an exit intent on Tempo by proving its inclusion in the `exitRoot`.

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

Zone-to-zone transfer is a composition of exit and deposit:

1. Zone A user submits an exit intent of type Swap and deposit.
2. After batch verification, the portal swaps the gas token of zone A into the gas token of zone B on the Tempo DEX.
3. The portal calls `ZonePortal.deposit` for zone B with the swap output.
4. The zone B sequencer consumes the deposit and credits the recipient on zone B.

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

## Open questions

- Should deposits be cancellable if not consumed within a timeout?
- Should the portal support batched exit finalization for gas efficiency?
- Should exit intents allow fee payment for DEX swaps in the output token?
