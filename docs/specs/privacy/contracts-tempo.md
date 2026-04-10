# Tempo-side Zone Contracts (Draft)

This document specifies the Tempo-side contract surface for zones.

These contracts live on Tempo and are responsible for:

- zone creation and top-level registry state
- escrow of deposited TIP-20 tokens
- batch verification and settlement progress
- withdrawal processing on Tempo
- callback-based interoperability through `ZoneMessenger`

For zone predeploys and zone-side execution entry points, see [Zone-side contracts](https://github.com/tempoxyz/zones/blob/docs/zones-specs-entrypoint/docs/specs/privacy/contracts-zone.md). For verifier semantics, proof inputs, and queue commitments, see [Zone Prover Design](https://github.com/tempoxyz/zones/blob/docs/zones-specs-entrypoint/docs/specs/privacy/prover-design.md). For execution-level token behavior inside the zone, see [Execution](https://github.com/tempoxyz/zones/blob/docs/zones-specs-entrypoint/docs/specs/privacy/execution.md).

## Shared Tempo-facing types

```solidity
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

struct TokenConfig {
    bool enabled;
    bool depositsActive;
}

struct Deposit {
    address token;
    address sender;
    address to;
    uint128 amount;
    bytes32 memo;
}

struct EncryptedDepositPayload {
    bytes32 ephemeralPubkeyX;
    uint8 ephemeralPubkeyYParity;
    bytes ciphertext;
    bytes12 nonce;
    bytes16 tag;
}

struct EncryptionKeyEntry {
    bytes32 x;
    uint8 yParity;
    uint64 activationBlock;
}

struct Withdrawal {
    address token;
    bytes32 senderTag;
    address to;
    uint128 amount;
    uint128 fee;
    bytes32 memo;
    uint64 gasLimit;
    address fallbackRecipient;
    bytes callbackData;
    bytes encryptedSender;
}

struct BlockTransition {
    bytes32 prevBlockHash;
    bytes32 nextBlockHash;
}

struct DepositQueueTransition {
    bytes32 prevProcessedHash;
    bytes32 nextProcessedHash;
}
```

`senderTag` and `encryptedSender` are part of the authenticated-withdrawal design. The zone-side generation rules are specified in [Zone-side contracts](https://github.com/tempoxyz/zones/blob/docs/zones-specs-entrypoint/docs/specs/privacy/contracts-zone.md#authenticated-withdrawals).

## `ZoneFactory`

`ZoneFactory` is the shared Tempo contract that creates zones and maintains the global zone registry.

At zone creation time it:

- assigns the next `zoneId`
- deploys a per-zone `ZonePortal`
- deploys a per-zone `ZoneMessenger`
- stores the initial verifier, sequencer, and genesis parameters

During upgrades it also acts as the fan-out point for verifier rotation across all registered portals.

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

    function createZone(CreateZoneParams calldata params)
        external
        returns (uint32 zoneId, address portal);

    function zoneCount() external view returns (uint32);
    function zones(uint32 zoneId) external view returns (ZoneInfo memory);
    function isZonePortal(address portal) external view returns (bool);

    function protocolVersion() external view returns (uint64);
    function setForkVerifier(address forkVerifier) external;
}
```

The exact hard-fork sequencing and verifier-slot rotation rules are specified in [Upgrades](https://github.com/tempoxyz/zones/blob/docs/zones-specs-entrypoint/docs/specs/privacy/upgrades.md).

## `ZonePortal`

`ZonePortal` is the main Tempo-side settlement contract for a zone. It escrows deposits, tracks proven progress, accepts batch submissions, and processes withdrawals.

Conceptually it owns five groups of state:

- **zone identity**: `zoneId`, `sequencer`, `verifier`, genesis anchor
- **token registry**: enabled tokens plus per-token deposit pause state
- **deposit progress**: `currentDepositQueueHash`
- **proof progress**: `blockHash`, `withdrawalBatchIndex`, `lastSyncedTempoBlockNumber`
- **withdrawal queue**: the fixed-size ring buffer used for Tempo-side dequeue operations

### Token registry and fees

The portal's token registry is append-only for enablement:

- `enableToken(token)` permanently adds a token to the zone
- `pauseDeposits(token)` and `resumeDeposits(token)` only affect new deposits
- withdrawals remain available for any enabled token

The portal also holds the deposit-side gas rate:

- `zoneGasRate()` returns the current token-denominated deposit gas price
- `FIXED_DEPOSIT_GAS()` is fixed at `100,000`
- `calculateDepositFee()` computes the processing fee deducted before the deposit enters the queue

### Deposits and encrypted deposits

Regular deposits:

- transfer the token into portal escrow
- deduct the processing fee
- append the net deposit to `currentDepositQueueHash`

Encrypted deposits follow the same accounting path, but the Tempo-visible payload only reveals the public accounting fields. Recipient and memo are encrypted to the sequencer's published key.

The portal keeps a history of encryption keys so deposits can explicitly target the correct key version with `keyIndex`.

### Batch submission

`submitBatch(...)` is sequencer-only. The call:

1. checks that the submitted `prevBlockHash` matches the portal's stored `blockHash`
2. calls the verifier
3. updates `blockHash`, `withdrawalBatchIndex`, and `lastSyncedTempoBlockNumber`
4. appends the batch's `withdrawalQueueHash` to the Tempo-side withdrawal ring buffer when the batch contains withdrawals

The detailed proof obligations and queue-commitment semantics live in [Zone Prover Design](https://github.com/tempoxyz/zones/blob/docs/zones-specs-entrypoint/docs/specs/privacy/prover-design.md).

### Withdrawal processing

`processWithdrawal(withdrawal, remainingQueue)` is sequencer-only and removes the next withdrawal from the oldest withdrawal slot in the Tempo-side ring buffer.

There are two execution modes:

- `gasLimit == 0`: direct TIP-20 transfer on Tempo
- `gasLimit > 0`: route through `ZoneMessenger`, which performs transfer-plus-callback atomically

The withdrawal is popped from the Tempo-side queue regardless of success or failure. On failure, the portal re-deposits the amount back into the same zone for `fallbackRecipient`.

### Sequencer transfer

Sequencer handoff is a two-step process:

1. `transferSequencer(newSequencer)` nominates a pending sequencer
2. `acceptSequencer()` finalizes the handoff from the new address

This keeps Tempo as the single source of truth for operator control.

### Interface

```solidity
interface IZonePortal {
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

    function setZoneGasRate(uint128 zoneGasRate) external;
    function calculateDepositFee() external view returns (uint128 fee);

    function deposit(address token, address to, uint128 amount, bytes32 memo)
        external
        returns (bytes32 newCurrentDepositQueueHash);

    function depositEncrypted(
        address token,
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata encrypted
    ) external returns (bytes32 newCurrentDepositQueueHash);

    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);
    function setSequencerEncryptionKey(
        bytes32 x,
        uint8 yParity,
        uint8 popV,
        bytes32 popR,
        bytes32 popS
    ) external;
    function encryptionKeyCount() external view returns (uint256);
    function encryptionKeyAt(uint256 index) external view returns (EncryptionKeyEntry memory);
    function encryptionKeyAtBlock(uint64 tempoBlockNumber)
        external
        view
        returns (bytes32 x, uint8 yParity, uint256 keyIndex);
    function isEncryptionKeyValid(uint256 keyIndex)
        external
        view
        returns (bool valid, uint64 expiresAtBlock);

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

## `ZoneMessenger`

Each zone has a dedicated `ZoneMessenger` on Tempo. The portal gives the messenger approval for enabled tokens so callback withdrawals can pull funds from portal escrow.

`ZoneMessenger` is the Tempo-side composition hook for zones. It is what lets a withdrawal land in another contract rather than just in a wallet.

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

`relayMessage(...)` is only callable by the portal. It performs:

1. `transferFrom(portal, target, amount)`
2. the callback into `target`

Those two operations are atomic. If the callback reverts, the transfer also reverts and the portal can bounce the withdrawal back into the zone.

## `IWithdrawalReceiver`

Tempo contracts that want to receive callback withdrawals must implement:

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

The receiver must return `IWithdrawalReceiver.onWithdrawalReceived.selector`. Any revert or wrong return value is treated as withdrawal failure.

## Withdrawal failure behavior

Tempo-side withdrawal execution can fail for several reasons:

- TIP-20 transfer failure
- TIP-403 transfer-policy rejection
- token pause
- callback revert
- callback rejection through a wrong return selector

When this happens, the portal enqueues a regular deposit back into the same zone for `fallbackRecipient`. The amount returns to the zone through normal deposit processing, so the withdrawal queue can continue moving.

### TIP-403 considerations

TIP-20 transfers on Tempo continue to obey Tempo's TIP-403 policy model:

- both `from` and `to` authorization checks still apply
- policy changes on Tempo can therefore affect zone withdrawal execution

For smooth operation:

- zone creators should prefer tokens with the default always-allow transfer policy when possible
- if a restricted policy is used, the portal must remain authorized under that policy
- users should choose a `fallbackRecipient` they control, since policy changes can cause a Tempo-side withdrawal to bounce back
