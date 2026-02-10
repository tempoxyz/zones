# Deposit Flow: Remaining Work

Tracks gaps between the spec (`docs/pages/protocol/privacy/overview.md`) and the Rust implementation (`crates/tempo-zone/src/l1.rs`).

## Done

- [x] Deposit struct updated to match spec: `{sender, to, amount: u128, memo: B256}`
- [x] DepositEnqueued event ABI matches struct
- [x] Deposit queue hash chain: `keccak256(abi.encode(deposit, prevHash))`
- [x] `DepositQueueState` tracks `current_hash` and `pending_deposits`
- [x] `DepositQueueTransition` struct: `{prev_processed_hash, next_processed_hash}`
- [x] `DepositProcessor` tracks zone-side `processed_hash` and produces transitions
- [x] ZonePortal address is configurable via `--l1.portal-address` / `L1_PORTAL_ADDRESS`

## Remaining

### Deposit Fees (needs discussion)

The spec says:
- Fee = `FIXED_DEPOSIT_GAS (100,000) * zoneGasRate`
- Fee deducted from deposit amount on L1, paid to sequencer immediately
- Deposit queue stores net amount (`amount - fee`)

**Open question**: If the L1 portal already deducts the fee and the `DepositEnqueued` event emits the net amount, the zone side doesn't need fee logic — it just processes what it receives. Does the zone need to independently validate fees, or trust the L1 portal?

### Unified Deposit Queue with DepositType Discriminator

The spec defines `DepositType { Regular, Encrypted }` with type-discriminated hashing:
```
keccak256(abi.encode(DepositType.Regular, deposit, prevHash))
keccak256(abi.encode(DepositType.Encrypted, encryptedDeposit, prevHash))
```

Current implementation only handles regular deposits (no type discriminator in hash). When encrypted deposits are added, the hash chain must include the `DepositType` prefix.

### Encrypted Deposits

Full ECIES pipeline needed:
- `EncryptedDeposit` struct: `{sender, amount, keyIndex, encrypted: EncryptedDepositPayload}`
- `EncryptedDepositPayload`: `{ephemeralPubkeyX, ephemeralPubkeyYParity, ciphertext, nonce, tag}`
- `DecryptionData`: `{sharedSecret, sharedSecretYParity, to, memo, cpProof}`
- L1 subscriber must handle `EncryptedDepositMade` events
- Zone-side decryption flow in `advanceTempo`
- Key history tracking (`keyIndex` lookup)
- Griefing prevention via Chaum-Pedersen proof of correct ECDH

### advanceTempo System Transaction

Zone-side entry point that:
1. Advances Tempo state via `TempoState.finalizeTempo(header)` (validates chain continuity)
2. Processes deposits from unified queue (regular + encrypted)
3. Mints zone tokens to recipients
4. Validates hash chain against Tempo's `currentDepositQueueHash`
5. Updates `processedDepositQueueHash`

This is a sequencer-only system transaction at block start. Needs EVM execution integration.

### Bounce-Back Deposits

When withdrawals fail on Tempo (transfer revert, callback failure, TIP-403 policy, token pause):
- Portal enqueues a bounce-back deposit to `fallbackRecipient`
- Uses the same deposit queue: `currentDepositQueueHash = keccak256(abi.encode(deposit, currentDepositQueueHash))`
- L1 subscriber must handle `BounceBack` events
- Zone processes bounce-backs as regular deposits

### Precompiles

Two new precompiles required for encrypted deposit verification:

1. **Chaum-Pedersen Verify** (`0x1c00000000000000000000000000000000000100`)
   - Verifies proof that `sharedSecret = privSeq * ephemeralPub` without revealing `privSeq`
   - Input: ephemeralPub, sharedSecret, sequencerPub, proof (s, c)
   - ~8000 gas (2 EC mults + 2 EC adds + hash)

2. **AES-256-GCM Decrypt** (`0x1c00000000000000000000000000000000000101`)
   - Decrypts ciphertext and verifies GCM authentication tag
   - Input: key (32B), nonce (12B), ciphertext, AAD, tag (16B)
   - ~1000 gas base + ~500 per 32 bytes

HKDF-SHA256 is implemented in Solidity using the existing SHA256 precompile (0x02).

### WS Log Subscription Hardening

Current WS subscription has no reorg/finality handling:
- No confirmation depth tracking
- No reorg detection or rollback
- Deposits are enqueued immediately on log arrival
- The spec avoids this by anchoring to on-chain hash state rather than event indexing

Options:
1. Add confirmation depth (e.g., wait N blocks before considering deposit final)
2. Track finalized block number and only process deposits from finalized blocks
3. Move to polling with `eth_getLogs` + finalized block tag instead of WS subscription

### Event Alignment

The spec's `IZonePortal` defines these events (only `DepositEnqueued` is currently handled):
- `DepositMade` → renamed to `DepositEnqueued` in implementation
- `EncryptedDepositMade` — not yet handled
- `BounceBack` — not yet handled
- `BatchSubmitted` — not yet handled (observability)
