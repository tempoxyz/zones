# Zone-side Contracts (Draft)

This document specifies the zone-side contract surface.

These contracts are system contracts that live inside the zone and are responsible for:

- maintaining the zone's imported view of finalized Tempo state
- processing deposits into zone balances
- recording withdrawals back to Tempo
- exposing zone configuration derived from Tempo-side state
- enforcing token policy behavior inside the zone

## Fixed addresses and token model

Zones have four core system predeploys at fixed addresses:

- `TempoState` at `0x1c00000000000000000000000000000000000000`
- `ZoneInbox` at `0x1c00000000000000000000000000000000000001`
- `ZoneOutbox` at `0x1c00000000000000000000000000000000000002`
- `ZoneConfig` at `0x1c00000000000000000000000000000000000003`

In addition, each enabled TIP-20 token appears inside the zone at the same address it uses on Tempo.

### Zone token model

Zones do not have a token factory. Every zone token is a bridged representation of a Tempo TIP-20.

- enabling a token on the Tempo portal makes it available on the zone
- `ZoneInbox` mints on deposit
- `ZoneOutbox` burns on withdrawal
- no user or issuer can arbitrarily create new zone-side token contracts

This is what keeps zone-side token supply tied to Tempo-side escrow.

## Shared zone-side types

```solidity
interface IZoneToken {
    function mint(address to, uint256 amount) external;
    function burn(address from, uint256 amount) external;
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

enum DepositType {
    Regular,
    Encrypted
}

struct QueuedDeposit {
    DepositType depositType;
    bytes depositData;
}

struct ChaumPedersenProof {
    bytes32 s;
    bytes32 c;
}

struct DecryptionData {
    bytes32 sharedSecret;
    uint8 sharedSecretYParity;
    address to;
    bytes32 memo;
    ChaumPedersenProof cpProof;
}

struct LastBatch {
    bytes32 withdrawalQueueHash;
    uint64 withdrawalBatchIndex;
}
```

## `ZoneConfig`

`ZoneConfig` is the zone's configuration oracle. It reads sequencer and token-enablement state from the finalized Tempo-side `ZonePortal` through `TempoState`.

That makes Tempo the single source of truth for:

- current sequencer
- pending sequencer
- enabled tokens
- current sequencer encryption key

```solidity
interface IZoneConfig {
    function tempoPortal() external view returns (address);
    function tempoState() external view returns (ITempoState);

    function sequencer() external view returns (address);
    function pendingSequencer() external view returns (address);
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);

    function isSequencer(address account) external view returns (bool);
    function isEnabledToken(address token) external view returns (bool);
}
```

Zone system contracts use `ZoneConfig` for sequencer-only checks instead of carrying their own mutable operator state.

## `TempoState`

`TempoState` stores the zone's imported view of finalized Tempo headers and provides restricted Tempo storage reads for system contracts.

```solidity
interface ITempoState {
    event TempoBlockFinalized(bytes32 indexed blockHash, uint64 indexed blockNumber, bytes32 stateRoot);

    function tempoBlockHash() external view returns (bytes32);
    function tempoStateRoot() external view returns (bytes32);
    function tempoBlockNumber() external view returns (uint64);

    function finalizeTempo(bytes calldata header) external;

    function readTempoStorageSlot(address account, bytes32 slot)
        external
        view
        returns (bytes32);

    function readTempoStorageSlots(address account, bytes32[] calldata slots)
        external
        view
        returns (bytes32[] memory);
}
```

Important properties:

- `finalizeTempo(header)` is only for the zone's system flow, reached through `ZoneInbox.advanceTempo(...)`
- raw Tempo-state reads are restricted to system contracts
- user transactions cannot directly query arbitrary Tempo storage

`TempoState` commits to the full RLP-encoded Tempo header hash, even though only the fields needed by zone execution are exposed directly to zone logic.

## `TIP403Registry`

The zone also exposes a read-only `TIP403Registry` at the same address as Tempo.

It does not own policy state locally. Instead, `isAuthorized(...)` reads Tempo policy state through `TempoState`, which ensures zone-side TIP-20 transfers continue to obey Tempo's TIP-403 policy configuration.

## `ZoneInbox`

`ZoneInbox` is the zone-side entry point for importing finalized Tempo state and consuming deposits from the deposit queue.

`advanceTempo(...)` is sequencer-only and atomically:

1. finalizes a Tempo header in `TempoState`
2. processes deposits in queue order
3. mints the correct zone-side balances
4. verifies that the resulting processed hash matches the Tempo-side deposit queue head

```solidity
interface IZoneInbox {
    function config() external view returns (IZoneConfig);
    function tempoPortal() external view returns (address);
    function tempoState() external view returns (ITempoState);
    function processedDepositQueueHash() external view returns (bytes32);

    function advanceTempo(
        bytes calldata header,
        QueuedDeposit[] calldata deposits,
        DecryptionData[] calldata decryptions
    ) external;
}
```

Encrypted deposits are validated here as well. If decryption succeeds, the deposit mints to the decrypted recipient. If decryption fails, the deposit mints back to the depositor's zone address so the queue can keep moving.

## `ZoneOutbox`

`ZoneOutbox` is the zone-side withdrawal entry point. Users approve it to spend zone tokens, then call `requestWithdrawal(...)` to begin an exit back to Tempo.

The contract:

- calculates and charges the Tempo-side processing fee
- burns `amount + fee`
- stores the withdrawal in pending state
- exposes the last finalized batch commitment for proof access

At the end of the final block in a batch, the sequencer calls `finalizeWithdrawalBatch(count)` to turn pending withdrawals into the single `withdrawalQueueHash` that Tempo will later enqueue.

```solidity
interface IZoneOutbox {
    function MAX_CALLBACK_DATA_SIZE() external view returns (uint256);

    function tempoGasRate() external view returns (uint128);
    function nextWithdrawalIndex() external view returns (uint64);
    function withdrawalBatchIndex() external view returns (uint64);
    function lastBatch() external view returns (LastBatch memory);
    function pendingWithdrawalsCount() external view returns (uint256);

    function maxWithdrawalsPerBlock() external view returns (uint256);
    function setTempoGasRate(uint128 tempoGasRate) external;
    function setMaxWithdrawalsPerBlock(uint256 maxWithdrawalsPerBlock) external;
    function calculateWithdrawalFee(uint64 gasLimit) external view returns (uint128);

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

    function finalizeWithdrawalBatch(uint256 count)
        external
        returns (bytes32 withdrawalQueueHash);
}
```

`finalizeWithdrawalBatch(...)` must appear exactly once in the final block of a batch, even if `count == 0`. Intermediate blocks must not call it.

## Authenticated withdrawals

Authenticated withdrawals were present in the old overview and are now specified here because they are generated from `ZoneOutbox.requestWithdrawal(...)` and consumed by Tempo-side withdrawal processing.

### Problem

Zone transactions are private, but Tempo-side withdrawal processing is public. If the Tempo-side `Withdrawal` struct contained a plaintext `sender`, then `processWithdrawal(...)` calldata would reveal the sender identity to anyone watching Tempo.

### Mechanism

The public Tempo-side withdrawal replaces plaintext sender information with:

- `senderTag = keccak256(abi.encodePacked(sender, txHash))`
- `encryptedSender`, which is an optional encrypted reveal of `(sender, txHash)`

`txHash` is private to the zone and acts as the blinding factor. Because the sender does not know the final zone transaction hash at signing time, the sequencer computes `senderTag` and `encryptedSender` after executing the withdrawal request and before emitting the batch commitment.

### `revealTo`

`requestWithdrawal(...)` takes an optional `revealTo` public key:

- if `revealTo` is empty, no encrypted reveal is attached and the sender can disclose `txHash` manually off-chain
- if `revealTo` is present, the sequencer encrypts `(sender, txHash)` to that key and stores the result in the public Tempo-side `Withdrawal`

The `revealTo` key stays in zone-side pending withdrawal state. Only the encrypted output is surfaced on Tempo.

### Encrypted sender format

When present, `encryptedSender` is:

```text
ephemeralPubKey (33 bytes) || ciphertext (52 bytes) || mac (16 bytes)
```

The holder of the private key corresponding to `revealTo` can decrypt it and recover `(sender, txHash)`.

### Selective disclosure

Two disclosure modes exist:

- **Manual reveal**: the sender discloses `txHash` off-chain, and a verifier checks `keccak256(abi.encodePacked(sender, txHash)) == senderTag`
- **Encrypted reveal**: the holder of the `revealTo` private key decrypts `encryptedSender` and checks the same relation

This lets the public remain blind while still allowing recipients, counterparties, or another zone sequencer to validate sender identity when needed.

### Zone-to-zone transfers

For zone-to-zone transfers, the sender can set `revealTo` to the destination zone sequencer's public key.

That lets the destination sequencer:

1. observe the public Tempo-side withdrawal
2. decrypt `encryptedSender`
3. recover `(sender, txHash)`
4. verify it against `senderTag`

This is what allows sender-aware processing on the destination zone without revealing the sender to the public chain.

### Trust model

The sequencer is trusted to compute `senderTag` and `encryptedSender` honestly. Those values are committed into the withdrawal queue hash and therefore into the proved batch, but the current prover does not independently verify the sender-tag preimage or encryption correctness.

That means authenticated withdrawals add a modest new trust assumption on top of the existing sequencer trust model.

In a future design, `senderTag` computation could move into the proof itself while leaving encryption sequencer-mediated.

### Impact on callbacks

For callback withdrawals, `IWithdrawalReceiver.onWithdrawalReceived(...)` receives `senderTag` rather than a plaintext sender address.

Contracts that need sender identity can recover it off-chain through `encryptedSender` or through a separately disclosed `txHash`.

### Zone-side events

`WithdrawalRequested` inside the zone can still include the plaintext sender because zone events are private. The privacy problem only arises once the public Tempo-side withdrawal object is constructed.
