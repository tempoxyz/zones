# Cryptography review checklist

This document lists the cryptographic constructions in the zone privacy specification that require expert review. The focus is on spec-level correctness — whether the protocols are sound, the parameters are safe, and the trust assumptions are clearly stated.

For each construction, we include the security model and an extract of the spec sections a reviewer should read.

---

## 1. Encrypted deposits (ECIES + Chaum-Pedersen)

### Security model

**Functionality**: Users can deposit tokens into a zone while hiding the recipient address and memo. The construction provides three guarantees:

1. **Correct encryption → recipient is credited.** If the user correctly encrypts `(to, memo)` to the sequencer's published encryption key, the zone credits `to` with the deposited amount. If the mint to `to` fails (e.g. the recipient is blocked by TIP-403 token policy), the zone falls back to crediting the depositor instead. The sequencer cannot selectively reject a correctly encrypted deposit without halting the processing of *all* deposits — the deposit queue is an ordered hash chain, so skipping one deposit means all subsequent deposits stall.

2. **Incorrect encryption → refund on L1.** If the ciphertext is malformed, encrypted to the wrong key, or otherwise fails decryption, the deposit amount is minted to the `sender` address on the zone (the same account that deposited on L1). The L1 funds remain escrowed in the portal. Chain progress is never blocked by invalid encrypted deposits.

3. **Sequencer cannot lie about decryption.** The sequencer provides the ECDH shared secret and a Chaum-Pedersen DLEQ proof that it was correctly derived. The AES-GCM authentication tag then verifies the decrypted plaintext. Together, these prevent the sequencer from claiming a deposit decrypts to a different `(to, memo)` — the GCM tag would fail. The only attack is refusing to process at all, which triggers the refund path.

**Trust assumptions**:
- The sequencer's encryption key is registered on-chain with a proof-of-possession (ECDSA signature). Users trust this key is honestly generated.
- The `token`, `sender`, and `amount` fields are always public (needed for on-chain escrow accounting). Only `to` and `memo` are encrypted.
- A compromised sequencer private key exposes all past and future encrypted deposit recipients until key rotation. Old keys remain valid for a grace period (86,400 blocks ≈ 1 day). Deposits using a compromised old key during the grace period are retroactively exposed.

**Cryptographic components** (reviewed together because they form a single verification pipeline):
- **ECIES**: secp256k1 ECDH + HKDF-SHA256 + AES-256-GCM — encrypts `(to, memo)`.
- **Chaum-Pedersen DLEQ proof**: non-interactive sigma protocol (Fiat-Shamir) — proves the sequencer used the correct private key for ECDH, preventing griefing with deposits encrypted to the wrong key.
- **Secp256k1 point validation**: Euler's criterion via MODEXP precompile — validates ephemeral public keys on the portal to prevent invalid-point griefing.

### Spec extract

**Encryption scheme** (ECIES with secp256k1):

1. Sequencer publishes a secp256k1 encryption public key via `setSequencerEncryptionKey(x, yParity, popV, popR, popS)` with a proof-of-possession (ECDSA signature over `keccak256(abi.encode(portalAddress, x, yParity))` by the corresponding private key).
2. User generates an ephemeral keypair and derives a shared secret via ECDH.
3. AES-256 key derived via HKDF-SHA256 with salt `"ecies-aes-key"` and info `abi.encodePacked(tempoPortal, keyIndex, ephemeralPubkeyX)`.
4. Plaintext `(to || memo || padding)` = 64 bytes (20 addr + 32 memo + 12 zero padding) encrypted with AES-256-GCM (empty AAD, user-chosen 12-byte nonce).
5. User calls `depositEncrypted(token, amount, keyIndex, encryptedPayload)` on the portal.

**On-chain types** (`IZone.sol`):

```solidity
struct EncryptedDepositPayload {
    bytes32 ephemeralPubkeyX;     // Ephemeral public key X coordinate (for ECDH)
    uint8 ephemeralPubkeyYParity; // Y coordinate parity (0x02 or 0x03)
    bytes ciphertext;             // AES-256-GCM encrypted (to || memo || padding)
    bytes12 nonce;                // GCM nonce
    bytes16 tag;                  // GCM authentication tag
}

struct EncryptedDeposit {
    address token;               // TIP-20 token (public, for escrow accounting)
    address sender;              // Depositor (public, for refunds)
    uint128 amount;              // Amount (public, for accounting)
    uint256 keyIndex;            // Index of encryption key used (specified by depositor)
    EncryptedDepositPayload encrypted;
}

struct DecryptionData {
    bytes32 sharedSecret;        // ECDH shared secret (x-coordinate of privSeq * ephemeralPub)
    uint8 sharedSecretYParity;   // Y coordinate parity of the shared secret point (0x02 or 0x03)
    ChaumPedersenProof cpProof;  // Proof of correct shared secret derivation
}

struct ChaumPedersenProof {
    bytes32 s; // Response: s = r + c * privSeq (mod n)
    bytes32 c; // Challenge: c = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)
}
```

**On-chain verification pipeline** (`ZoneInbox.sol`, `advanceTempo`):

```solidity
// Step 1: Verify Chaum-Pedersen DLEQ proof — proves shared secret was derived correctly
(bytes32 seqPubX, uint8 seqPubYParity) = _readEncryptionKey(ed.keyIndex);
bool proofValid = IChaumPedersenVerify(CHAUM_PEDERSEN_VERIFY).verifyProof(
    ed.encrypted.ephemeralPubkeyX,
    ed.encrypted.ephemeralPubkeyYParity,
    dec.sharedSecret,
    dec.sharedSecretYParity,
    seqPubX,
    seqPubYParity,
    dec.cpProof
);
if (!proofValid) revert InvalidSharedSecretProof();

// Step 2: Derive AES key from shared secret using HKDF-SHA256
bytes32 aesKey = _hkdfSha256(
    dec.sharedSecret,
    "ecies-aes-key",
    abi.encodePacked(tempoPortal, ed.keyIndex, ed.encrypted.ephemeralPubkeyX)
);

// Step 3: Decrypt using AES-256-GCM precompile
(bytes memory decryptedPlaintext, bool valid) = IAesGcmDecrypt(AES_GCM_DECRYPT).decrypt(
    aesKey, ed.encrypted.nonce, ed.encrypted.ciphertext, "", ed.encrypted.tag
);

// Step 4: Decode the decrypted (to, memo) from the plaintext
address decryptedTo;
bytes32 decryptedMemo;
if (valid && decryptedPlaintext.length == ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE) {
    (decryptedTo, decryptedMemo) =
        EncryptedDepositLib.decodePlaintext(decryptedPlaintext);
} else {
    valid = false;
}

// Step 5: Mint to decrypted recipient on success, or refund to sender on failure.
// If the mint to decryptedTo fails (e.g. TIP-403 policy rejects the recipient),
// fall back to crediting the depositor.
if (!valid) {
    IZoneToken(ed.token).mint(ed.sender, ed.amount);
} else {
    try IZoneToken(ed.token).mint(decryptedTo, ed.amount) {} catch {
        IZoneToken(ed.token).mint(ed.sender, ed.amount);
    }
}
```

The sequencer's public key (`seqPubX`, `seqPubYParity`) is looked up on-chain via `_readEncryptionKey(ed.keyIndex)`, which reads from the portal's storage through `TempoState.readTempoStorageSlot`. It is not supplied by the sequencer in `DecryptionData`, preventing substitution attacks.

**Chaum-Pedersen DLEQ protocol**:

Proves knowledge of `privSeq` such that `pubSeq = privSeq * G` AND `sharedSecretPoint = privSeq * ephemeralPub`:

1. **Prover (sequencer) computes off-chain:**
   - Pick random `r`
   - `R1 = r * G`
   - `R2 = r * ephemeralPub`
   - `c = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)` (Fiat-Shamir challenge)
   - `s = r + c * privSeq (mod n)`
   - Proof is `(s, c)`

2. **Verifier (on-chain precompile at `0x1C00...0100`) checks:**
   - Reconstruct: `R1 = s*G - c*pubSeq`
   - Reconstruct: `R2 = s*ephemeralPub - c*sharedSecretPoint`
   - Recompute: `c' = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)`
   - Verify: `c == c'`

**Precompile interface** (`IZone.sol`):

```solidity
interface IChaumPedersenVerify {
    function verifyProof(
        bytes32 ephemeralPubX,
        uint8 ephemeralPubYParity,
        bytes32 sharedSecret,
        uint8 sharedSecretYParity,
        bytes32 sequencerPubX,
        uint8 sequencerPubYParity,
        ChaumPedersenProof calldata proof
    ) external view returns (bool valid);
}
```

**HKDF-SHA256 implementation** (`ZoneInbox.sol`):

```solidity
function _hkdfSha256(bytes32 ikm, bytes memory salt, bytes memory info)
    internal view returns (bytes32 okm)
{
    // HKDF-Extract: PRK = HMAC-SHA256(salt, IKM)
    bytes32 prk = _hmacSha256(salt, abi.encodePacked(ikm));
    // HKDF-Expand: OKM = HMAC-SHA256(PRK, info || 0x01)
    bytes memory expandInput = bytes.concat(info, hex"01");
    okm = _hmacSha256(abi.encodePacked(prk), expandInput);
}
```

**Ephemeral pubkey validation** (`ZonePortal.sol`):

The portal validates the ephemeral public key X coordinate is on secp256k1 using Euler's criterion via the MODEXP precompile: `(x³ + 7)^((p-1)/2) ≡ 1 (mod p)`. This prevents griefing with invalid points that would make ECDH and Chaum-Pedersen proofs impossible.

**Key rotation** (`ZonePortal.sol`):

Old encryption keys expire after `ENCRYPTION_KEY_GRACE_PERIOD = 86,400` blocks (~1 day at 1s block time). The current key never expires. Users specify `keyIndex` at signing time to avoid race conditions during rotation. Deposits using expired keys are rejected with `EncryptionKeyExpired`.

---

## 2. Authenticated withdrawals — sender tag

### Security model

**Functionality**: Hide the identity of the withdrawal sender from public observers on Tempo Mainnet, while allowing the sender to selectively disclose their identity to chosen parties.

**Trust assumptions**:
- The sequencer computes `senderTag` and includes it in the `Withdrawal` struct. The struct is hashed into the withdrawal queue chain committed in the batch proof. **The sequencer is trusted to compute the tag correctly.** A malicious sequencer could forge tags attributing withdrawals to wrong senders, or produce unverifiable tags. The batch proof would still be valid since the prover does not verify the tag's preimage.
- This is a modest extension of the existing trust model: the sequencer is already trusted for liveness, transaction ordering, and withdrawal processing.
- The blinding factor `txHash` is known to the sequencer and anyone with zone data access. The threat model relies on zone transaction data not being published on L1.
- To upgrade to trustless sender authentication, `senderTag` computation can be moved into the ZK circuit. The encryption would remain sequencer-mediated.

**Threat surface**:
- An observer who learns `txHash` (e.g., from a compromised sequencer) can deanonymize the sender.
- The commitment is hiding under the assumption that `txHash` is uniformly random and secret. Since `txHash = keccak256(transaction)`, it is uniformly distributed, but secrecy depends entirely on zone data privacy.

### Spec extract

**Sender tag computation** (overview.md §"Authenticated withdrawals", `ZoneOutbox.sol` line 362):

```
senderTag = keccak256(abi.encodePacked(sender, txHash))
```

where `sender` is the address that called `requestWithdrawal` on the zone and `txHash` is the hash of that zone transaction. The `txHash` acts as a 32-byte blinding factor — it is private to the zone and known only to the sender and the sequencer.

**On-chain construction** (`ZoneOutbox.sol`, `finalizeWithdrawalBatch`):

```solidity
Withdrawal memory w = Withdrawal({
    token: pendingWithdrawal.token,
    senderTag: keccak256(
        abi.encodePacked(pendingWithdrawal.sender, pendingWithdrawal.txHash)
    ),
    to: pendingWithdrawal.to,
    amount: pendingWithdrawal.amount,
    fee: pendingWithdrawal.fee,
    memo: pendingWithdrawal.memo,
    gasLimit: pendingWithdrawal.gasLimit,
    fallbackRecipient: pendingWithdrawal.fallbackRecipient,
    callbackData: pendingWithdrawal.callbackData,
    encryptedSender: encryptedSender
});
```

The `txHash` is obtained from the `ZoneTxContext` precompile (`0x1c00...0005`) at withdrawal request time:

```solidity
bytes32 txHash = IZoneTxContext(ZONE_TX_CONTEXT).currentTxHash();
```

**Selective disclosure** (overview.md §"Selective disclosure"):

- **Manual reveal**: sender reveals `txHash` off-chain. Verifier checks `keccak256(abi.encodePacked(sender, txHash)) == senderTag`.
- **Encrypted reveal**: if `revealTo` was specified, the holder of the `revealTo` private key decrypts `encryptedSender` to obtain `(sender, txHash)` and verifies against `senderTag`.

---

## 3. Authenticated withdrawals — encrypted sender reveal

### Security model

**Functionality**: Enable automated sender disclosure for cross-zone transfers. The sequencer encrypts `(sender, txHash)` to a `revealTo` public key specified by the sender, so the holder of the corresponding private key can learn the sender's identity without off-chain coordination.

**Trust assumptions**:
- The sequencer is trusted to encrypt correctly. A malicious sequencer could encrypt garbage or use a different key. This is acceptable since the sequencer already knows `sender` and `txHash` and could withhold them.
- The sender cannot perform the encryption themselves because `txHash` depends on the transaction contents (circular dependency). The sequencer encrypts post-hoc.
- Cross-zone scenario: if Zone B's sequencer holds the `revealTo` private key and is compromised, all sender identities for transfers to Zone B are exposed.

**Threat surface**:
- The `encryptedSender` ciphertext is in L1 calldata (public). The ciphertext is fixed-length (113 bytes) to avoid length-based information leakage.
- The symmetric cipher and MAC used for the inner encryption are not fully specified in the overview document. The `ZoneOutbox.sol` defines the format but the KDF and cipher choice should be made explicit.

### Spec extract

**Withdrawal request** (`ZoneOutbox.sol`):

The sender specifies an optional `revealTo` compressed secp256k1 public key (33 bytes) when calling `requestWithdrawal`. The outbox validates the key:

```solidity
function _validateRevealTo(bytes memory revealTo) internal view {
    if (revealTo.length == 0) return;
    if (revealTo.length != REVEAL_TO_KEY_LENGTH) revert InvalidRevealTo();  // 33 bytes
    bytes1 prefix = revealTo[0];
    if (prefix != 0x02 && prefix != 0x03) revert InvalidRevealTo();
    bytes32 x;
    assembly { x := mload(add(revealTo, 33)) }
    if (!_isValidSecp256k1X(x)) revert InvalidRevealTo();
}
```

**Encrypted sender format** (`ZoneOutbox.sol`, overview.md §"Encrypted sender format"):

When `revealTo` is specified, `encryptedSender` is exactly 113 bytes:

```
ephemeralPubKey (33 bytes) || nonce (12 bytes) || ciphertext (52 bytes) || tag (16 bytes)
```

The sequencer generates an ephemeral key pair `(r, R = r*G)`, derives a shared secret `S = r * revealTo` (ECDH), and encrypts `abi.encodePacked(sender, txHash)` (52 bytes).

**Length validation** (`ZoneOutbox.sol`):

```solidity
uint256 public constant AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTH = 113;

function _validateEncryptedSender(bytes memory revealTo, bytes memory encryptedSender) internal pure {
    uint256 expectedLength = revealTo.length == 0 ? 0 : AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTH;
    if (encryptedSender.length != expectedLength) {
        revert InvalidEncryptedSenderLength(encryptedSender.length, expectedLength);
    }
}
```

**Zone-to-zone flow** (overview.md §"Zone-to-zone transfers"):

1. Sender on Zone A calls `requestWithdrawal` with `revealTo = pubKeySeqB`.
2. Zone A's sequencer computes `senderTag` and `encryptedSender`.
3. Withdrawal is proven and submitted to L1. `processWithdrawal` transfers tokens to Zone B's portal.
4. Zone B's sequencer reads `encryptedSender`, decrypts with its private key to learn `(sender, txHash)`.
5. Zone B verifies `keccak256(sender || txHash) == senderTag`.

---

## 4. RPC authorization tokens

### Security model

**Functionality**: Authenticate every RPC request to a zone, scoping all responses to the caller's account. Tokens are read-only credentials — no RPC method authenticated solely by a token may modify state (withdrawals require a full transaction signature).

**Trust assumptions**:
- The token hash uses raw `keccak256` (not EIP-191/712) because P256 and WebAuthn signers cannot produce EIP-191 prefixed signatures. The `"TempoZoneRPC"` magic prefix must provide sufficient domain separation.
- Tokens are replayable within their validity window by design. The spec states this is acceptable because they are read-only credentials. Stolen tokens cannot move funds.
- Unscoped tokens (`zoneId = 0`) are valid for any zone on the network. Since tokens are read-only, this limits exposure to read access across zones.
- Keychain Access Keys use the zone's own `AccountKeychain` instance (not mirrored from Tempo L1). Revocation must be honored within 1 second of the revoking block being imported.

**Threat surface**:
- A stolen token grants read access to the victim's account data (balances, transaction history, events) for up to 30 days.
- If the magic prefix collides with another signing context, a valid RPC token could be replayed as a different signed message, or vice versa.
- WebAuthn verification skips RP ID hash and origin validation. The challenge binding to `authorizationTokenHash` must be sufficient.

### Spec extract

**Token hash** (`rpc.md` §"Authorization tokens"):

```solidity
bytes32 authorizationTokenHash = keccak256(abi.encodePacked(
    bytes32(0x54656d706f5a6f6e65525043),  // "TempoZoneRPC" magic prefix
    uint8(version),                         // spec version (currently 0)
    uint32(zoneId),                         // zone this key is valid for (0 = unscoped)
    uint64(chainId),                        // zone chain ID (replay protection)
    uint64(issuedAt),                       // unix timestamp (seconds) of issuance
    uint64(expiresAt)                       // unix timestamp (seconds) of expiry
));
```

**Signature types** (`rpc.md` §"Signature types"):

| Type | Detection | Address derivation |
|------|-----------|-------------------|
| **secp256k1** | Exactly 65 bytes, no type prefix | `ecrecover` → address |
| **P256** | First byte `0x01`, 130 bytes total | Address from embedded pubkey |
| **WebAuthn** | First byte `0x02`, variable length (max 2KB) | Same as P256 |
| **Keychain** | First byte `0x03` (V1) or `0x04` (V2), variable length | Authenticated account is `user_address`, not signing key |

**Transport wire format** (`rpc.md` §"Transport"):

```
<signature bytes><version: 1 byte><zoneId: 4 bytes><chainId: 8 bytes><issuedAt: 8 bytes><expiresAt: 8 bytes>
```

The token fields are always exactly 29 bytes. The server reads the last 29 bytes as token fields, everything before is the signature. Parsing from the end avoids ambiguity with variable-length signatures.

**Validation rules** (`rpc.md` §"Validation"):

- `zoneId` must match the zone's ID or be `0` (unscoped).
- `chainId` must match `eth_chainId`.
- `expiresAt - issuedAt > 2,592,000` (30 days max) → reject.
- `expiresAt <= now` → reject.
- `issuedAt > now + 60` (60-second clock skew tolerance) → reject.
- Keychain: signing key must be active, non-revoked, non-expired in `AccountKeychain`.

**Keychain V2 signing hash** (`rpc.md` §"Keychain Access Keys"):

V2 binds `user_address` into the signing hash: inner signature is over `keccak256(0x04 || authorizationTokenHash || user_address)`. V1 signs the raw `authorizationTokenHash` directly.

**WebAuthn verification** (`rpc.md` §"WebAuthn"):

Verified: authenticatorData length, UP/UV flags, AT flag NOT set, ED flag NOT set, `clientDataJSON.type == "webauthn.get"`, challenge matches `authorizationTokenHash` (Base64URL), P256 signature valid.

Skipped: RP ID hash (no single relying party), `clientDataJSON.origin` (no canonical origin), signature counter (anti-cloning left to app layer).

---

## Files to review

| Area | Spec (documentation) | Solidity spec |
|------|---------------------|---------------|
| Encrypted deposits (ECIES + Chaum-Pedersen) | [overview.md](overview.md) §"Encrypted deposits", [prover-design.md](prover-design.md) | [IZone.sol](../../../specs/src/zone/IZone.sol), [ZoneInbox.sol](../../../specs/src/zone/ZoneInbox.sol), [EncryptedDeposit.sol](../../../specs/src/zone/EncryptedDeposit.sol) |
| Sender tag | [overview.md](overview.md) §"Authenticated withdrawals" | [IZone.sol](../../../specs/src/zone/IZone.sol) (`Withdrawal.senderTag`), [ZoneOutbox.sol](../../../specs/src/zone/ZoneOutbox.sol) |
| Encrypted sender | [overview.md](overview.md) §"Reveal key", §"Encrypted sender format" | [IZone.sol](../../../specs/src/zone/IZone.sol), [ZoneOutbox.sol](../../../specs/src/zone/ZoneOutbox.sol) |
| RPC auth tokens | [rpc.md](rpc.md) §"Authorization tokens" | — |
| Point validation | — | [ZonePortal.sol](../../../specs/src/zone/ZonePortal.sol) (`_isValidSecp256k1X`), [ZoneOutbox.sol](../../../specs/src/zone/ZoneOutbox.sol) (`_isValidSecp256k1X`) |
| Key rotation | [overview.md](overview.md) §"Encrypted deposits" | [ZonePortal.sol](../../../specs/src/zone/ZonePortal.sol) (`setSequencerEncryptionKey`, `isEncryptionKeyValid`) |
