# Symmetric TIP-20 Bouncebacks for Zone Deposits and Withdrawals

This document specifies symmetric bounceback behavior for TIP-20 transfer-policy-blocked transfers on zones, covering both deposits and withdrawals.

- **Spec ID**: N/A (zone protocol change)
- **Authors/Owners**: @dankrad
- **Status**: Draft
- **Related Specs**: TIP-20, TIP-403, Zone Portal, Zone Inbox/Outbox
- **Issue**: https://github.com/tempoxyz/zones/issues/167

---

# Overview

## Abstract

Currently, bouncebacks only exist on the withdrawal path: when a `processWithdrawal` call on the ZonePortal fails (e.g., TIP-20 recipient is blacklisted), the funds are re-deposited to the zone via `_enqueueBounceBack`. The deposit path has no such mechanism — if a deposit's recipient becomes blacklisted between deposit submission and zone-side processing, the zone-side mint fails with no recovery path.

This spec introduces **symmetric bouncebacks** so that both deposits and withdrawals have consistent, deterministic fund recovery behavior. It also requires a **bounceback address** on every deposit, moves TIP-403 policy enforcement into the portal (bypassing the TIP-20 blacklist for portal transfers), and distinguishes bounce-triggered deposits from regular deposits.

## Motivation

1. **No deposit recovery path**: If a plaintext deposit recipient is blacklisted on the zone between deposit and processing, the zone-side `mint` fails and funds are stuck in the portal escrow with no way to return them.
2. **Asymmetric behavior**: Withdrawals have bouncebacks; deposits do not. This creates confusion and edge cases, especially for cross-zone transfers via the `SwapAndDepositRouter`.
3. **Zone-to-zone transfer failures**: When a cross-zone deposit fails, there is no standard mechanism for the original sender to recover their funds.
4. **Policy enforcement timing**: Currently the portal checks TIP-403 at deposit time (`deposit()`), but the policy state can change between deposit and zone-side processing. Validation should happen at initiation with guaranteed bounceback on failure, not at processing time with stuck funds.

---

# Specification

## 1. Bounceback Address on Deposits

### 1.1 `deposit()` signature change

The `deposit` function on `ZonePortal` gains a required `bouncebackAddress` parameter. This address specifies where funds should be returned on the zone if the deposit fails at processing time (e.g., the recipient is blacklisted on the zone).

```solidity
function deposit(
    address token,
    address to,
    uint128 amount,
    bytes32 memo,
    address bouncebackAddress  // NEW: required, zone-side refund destination
) external returns (bytes32 newCurrentDepositQueueHash);
```

- `bouncebackAddress` MUST NOT be `address(0)`.
- The `bouncebackAddress` is validated against TIP-403 policy at deposit time (see §3). If the bounceback address is not authorized, the deposit reverts. This guarantees that if the deposit fails on the zone, the bounceback will succeed.
- The `bouncebackAddress` is stored in the `Deposit` struct and included in the deposit queue hash chain.

### 1.2 `Deposit` struct change

```solidity
struct Deposit {
    address token;
    address sender;
    address to;
    uint128 amount;
    bytes32 memo;
    address bouncebackAddress;  // NEW: zone-side refund destination
}
```

### 1.3 `depositEncrypted()` signature change

Encrypted deposits also require a `bouncebackAddress`. Since the recipient is hidden, the bounceback address is always public (it is not encrypted).

```solidity
function depositEncrypted(
    address token,
    uint128 amount,
    uint256 keyIndex,
    EncryptedDepositPayload calldata encrypted,
    address bouncebackAddress  // NEW: required
) external returns (bytes32 newCurrentDepositQueueHash);
```

The `EncryptedDeposit` struct gains the same field:

```solidity
struct EncryptedDeposit {
    address token;
    address sender;
    uint128 amount;
    uint256 keyIndex;
    EncryptedDepositPayload encrypted;
    address bouncebackAddress;  // NEW
}
```

### 1.4 Bounceback deposits (portal-initiated)

Bounce-triggered re-deposits from `_enqueueBounceBack` do NOT require a bounceback address — they use `address(0)` as a sentinel to indicate a bounce deposit. Bounce deposits cannot themselves bounce (they are terminal).

## 2. Zone-Side Deposit Bounceback (ZoneInbox)

### 2.1 Processing regular deposits

When `ZoneInbox.advanceTempo()` processes a regular deposit, if the zone-side mint fails (e.g., recipient is blacklisted on the zone-side TIP-20), the deposit is **bounced back**:

1. Instead of minting to `deposit.to`, mint to `deposit.bouncebackAddress`.
2. Emit a `DepositBounced` event (not `DepositProcessed`).
3. The hash chain is still advanced normally — the bounceback does not alter the deposit queue hash.

```solidity
// In advanceTempo, for regular deposits:
bool mintSuccess = _tryMint(d.token, d.to, d.amount);
if (!mintSuccess) {
    // Bounce to bounceback address — this mint MUST succeed
    // (bounceback address was validated at deposit time)
    IZoneToken(d.token).mint(d.bouncebackAddress, d.amount);
    emit DepositBounced(currentHash, d.sender, d.to, d.bouncebackAddress, d.token, d.amount);
} else {
    emit DepositProcessed(currentHash, d.sender, d.to, d.token, d.amount, d.memo);
}
```

### 2.2 Processing encrypted deposits

For encrypted deposits, the `bouncebackAddress` is used for ALL failure cases:

- If decryption succeeds but the zone-side mint to `dec.to` fails → mint to `ed.bouncebackAddress`.
- If decryption fails → mint to `ed.bouncebackAddress` (NOT `ed.sender`).

> **Rationale**: Using `bouncebackAddress` consistently (instead of `ed.sender`) is critical for cross-zone transfers. When a deposit is initiated via `SwapAndDepositRouter`, the `sender` is the router contract address on Tempo — not the user. Minting to the router's address on the zone would effectively lose the user's funds. The `bouncebackAddress` is always the user's chosen recovery destination.

### 2.3 Bounce deposits are terminal

When processing a deposit where `bouncebackAddress == address(0)` (indicating it is itself a bounce-triggered re-deposit from the portal's `_enqueueBounceBack`), the mint MUST succeed. If it fails, the system is in an inconsistent state and the transaction reverts. This prevents infinite bounce loops.

## 3. TIP-403 Policy Validation at Deposit Time

### 3.1 Validate at initiation, not processing

The portal validates both the `to` address and the `bouncebackAddress` against TIP-403 at deposit time:

```solidity
uint64 policyId = ITIP20(token).transferPolicyId();

// Validate recipient can receive mints (isAuthorizedMintRecipient, not isAuthorizedRecipient,
// because zone-side processing mints tokens — compound policies may have different rules
// for transfer recipients vs mint recipients)
if (!TIP403_REGISTRY.isAuthorizedMintRecipient(policyId, to)) {
    revert DepositPolicyForbids();
}

// Validate bounceback address can also receive mints
if (!TIP403_REGISTRY.isAuthorizedMintRecipient(policyId, bouncebackAddress)) {
    revert BouncebackPolicyForbids();
}
```

> **Note**: For encrypted deposits, the `to` address is hidden at deposit time and cannot be validated. Only the `bouncebackAddress` is validated. The `to` validation happens implicitly at zone-side processing time — if the decrypted recipient is not authorized, the deposit bounces.

This means:
- If the recipient is blacklisted at deposit time → deposit reverts (no funds move).
- If the recipient becomes blacklisted after deposit but before zone processing → deposit bounces back to `bouncebackAddress` on the zone.
- The `bouncebackAddress` is guaranteed to be policy-authorized at deposit time, so the bounceback mint will succeed.

### 3.2 No re-validation at processing time

The zone-side `ZoneInbox` does NOT re-check TIP-403 for the `bouncebackAddress`. The validation at deposit time is sufficient, and the bounceback address is considered immutably valid for that deposit.

> **Rationale**: If the bounceback address itself becomes blacklisted between deposit and processing, the zone-side mint will still fail (because the zone-side TIP-20 enforces its own policy). This is an acceptable edge case — the funds remain in the portal escrow, and the sequencer can coordinate off-chain recovery. The key invariant is that the bounceback address was valid at the time the user chose it.

## 4. Zone Portal Bypasses TIP-20 Transfer Policy (Narrow Scope)

### 4.1 Portal exemption from TIP-403

The `ZonePortal` contract needs a **narrow** exemption from TIP-20 transfer policy checks for specific outbound operations. This does NOT mean the portal bypasses all policy — only the following transfers require bypass:

1. **Withdrawal processing (`processWithdrawal`)**: The portal transfers tokens to withdrawal recipients. If the recipient is blacklisted by TIP-20 policy, the portal needs the transfer to either succeed (so it can deliver funds) or fail cleanly (so it can bounceback). The portal handles its own policy enforcement via the bounceback mechanism — the TIP-20 should not independently block this and prevent the bounceback from triggering.
2. **Fee transfers to sequencer**: The portal transfers deposit/withdrawal fees to the sequencer. The sequencer address must always be reachable for fees.

**Transfers that do NOT need bypass:**
- **User → portal deposits** (`transferFrom` in `deposit()`/`depositEncrypted()`): These should continue to use normal `transferFrom` with TIP-403 enforcement. The user's sender authorization should still be checked by the TIP-20.

### 4.2 Implementation approach

Two options (implementation decision, not part of this spec):

1. **`systemTransferFrom`**: TIP-20 already has `systemTransferFrom(from, to, amount)`. The portal/messenger could be authorized as system callers, but ONLY for the outbound transfers listed above. Care must be taken since `systemTransferFrom` currently has restricted callers (`TIP_FEE_MANAGER_ADDRESS`).

2. **Policy whitelist**: The zone factory adds the portal and messenger to the TIP-403 policy whitelist at zone creation time. This is simpler but couples zone creation with policy management.

> **Open question**: The exact mechanism for portal bypass needs further design. The key constraint is that the bypass must be narrow — only portal outbound transfers during withdrawal processing and fee payout, not arbitrary transfers.

## 5. Distinguishing Bounce vs Regular Withdrawals

### 5.1 Withdrawal struct: `isBounce` flag

The `Withdrawal` struct does not change. Bounce-triggered deposits and regular deposits are distinguished at the **deposit level**, not the withdrawal level. The current withdrawal bounceback logic in `processWithdrawal` already works correctly — it calls `_enqueueBounceBack` which creates a deposit with `sender: address(this)` and `bouncebackAddress: address(0)`.

### 5.2 Deposit type: `DepositType.BounceBack`

Add a new deposit type to distinguish bounce-back deposits in the hash chain:

```solidity
enum DepositType {
    Regular,
    Encrypted,
    BounceBack   // NEW: bounce-triggered re-deposit
}
```

The `_enqueueBounceBack` function uses this type:

```solidity
function _enqueueBounceBack(address _token, uint128 amount, address fallbackRecipient) internal {
    Deposit memory depositData = Deposit({
        token: _token,
        sender: address(this),
        to: fallbackRecipient,
        amount: amount,
        memo: bytes32(0),
        bouncebackAddress: address(0)  // terminal: cannot bounce again
    });

    bytes32 newHash = keccak256(
        abi.encode(DepositType.BounceBack, depositData, currentDepositQueueHash)
    );
    currentDepositQueueHash = newHash;

    emit BounceBack(newHash, fallbackRecipient, _token, amount);
}
```

On the zone side, `ZoneInbox` processes `DepositType.BounceBack` deposits identically to `DepositType.Regular` but:
- Emits `DepositBounceBackProcessed` instead of `DepositProcessed`.
- If the mint fails (e.g., bounceback recipient is now blacklisted on zone), the transaction **reverts** — there is no further fallback. This is a protocol-level failure requiring manual intervention.

## 6. Withdrawal `fallbackRecipient` Validation

### 6.1 TIP-403 validation at withdrawal request time

Currently, `ZoneOutbox.requestWithdrawal` only validates that `fallbackRecipient != address(0)`. With symmetric bouncebacks, a failed withdrawal creates a `DepositType.BounceBack` deposit that is **terminal** — if the mint to `fallbackRecipient` on the zone fails, the zone block reverts.

To prevent this DoS vector, `requestWithdrawal` SHOULD validate the `fallbackRecipient` against TIP-403 at request time:

```solidity
// In ZoneOutbox.requestWithdrawal:
// Read token's transfer policy from Tempo via ZoneConfig
uint64 policyId = /* read from L1 via TempoState */;
if (!isAuthorizedMintRecipient(policyId, fallbackRecipient)) {
    revert InvalidFallbackRecipient();
}
```

> **Note**: This requires the zone to read the TIP-403 policy state from Tempo. The exact mechanism (direct L1 read via `TempoState` or cached policy on the zone) is an implementation detail. The key requirement is that the `fallbackRecipient` is validated before the withdrawal enters the queue.

## 7. Zone-to-Zone Transfer Failure Handling

### 7.1 Cross-zone deposit failure via SwapAndDepositRouter

When a cross-zone transfer fails:

1. User on Zone A requests withdrawal to `SwapAndDepositRouter` on Tempo, targeting Zone B.
2. The withdrawal callback calls `ZonePortal(zoneB).deposit(...)` with the user's Zone B address as `to`.
3. If the deposit into Zone B fails (e.g., recipient blacklisted on Zone B), the entire `onWithdrawalReceived` callback reverts.
4. The portal processes this as a failed withdrawal callback → `_enqueueBounceBack` sends funds back to Zone A via `fallbackRecipient`.

This flow already works with the existing bounceback mechanism. The `fallbackRecipient` on the Zone A withdrawal request is the user's Zone A address, so they get their funds back on Zone A.

### 7.2 Cross-zone bounceback address

For the deposit into Zone B (step 2 above), the `SwapAndDepositRouter` should set the `bouncebackAddress` to a meaningful value. Since the router itself is on Tempo (not on Zone B), the `bouncebackAddress` should be set to the `sender`'s address from the withdrawal callback — this is the user's Zone A address which will also be a valid address on Zone B.

If the deposit into Zone B succeeds but later the zone-side processing fails (recipient blacklisted on Zone B after deposit), the funds are minted to the `bouncebackAddress` on Zone B. The user then holds funds on Zone B that they can withdraw back to Tempo.

---

# Contract Changes Summary

| Contract | Change |
|----------|--------|
| `IZone.sol` | Add `bouncebackAddress` to `Deposit` and `EncryptedDeposit` structs. Add `DepositType.BounceBack`. Add `DepositBounced`, `DepositBounceBackProcessed` events. Add `BouncebackPolicyForbids` error. |
| `ZonePortal.sol` | Update `deposit()` and `depositEncrypted()` to accept and validate `bouncebackAddress`. Update `_enqueueBounceBack` to use `DepositType.BounceBack`. Narrow TIP-20 policy bypass for outbound transfers only. |
| `ZoneInbox.sol` | Handle deposit mint failures by minting to `bouncebackAddress` (including encrypted decryption failures). Process `DepositType.BounceBack` deposits. Revert on bounce deposit mint failure. |
| `ZoneOutbox.sol` | Validate `fallbackRecipient` against TIP-403 mint-recipient policy at withdrawal request time. |
| `DepositQueueLib.sol` | Add `enqueueBounceBack` function for the new deposit type. |
| `SwapAndDepositRouter.sol` | Pass `bouncebackAddress` to `deposit()` and `depositEncrypted()`. |

---

# Invariants

1. **Every deposit has a bounceback address**: No deposit function exists without a `bouncebackAddress` parameter (except bounce-back deposits themselves which use `address(0)`).

2. **Bounceback address is policy-valid at deposit time**: The portal validates the `bouncebackAddress` against TIP-403 before accepting the deposit. This guarantees that at the time of deposit, the bounceback destination was valid.

3. **Bounce deposits are terminal**: A bounce-triggered deposit (`DepositType.BounceBack`) has `bouncebackAddress == address(0)` and cannot itself bounce. If its mint fails, the zone block reverts.

4. **No funds are stuck in escrow without a recovery path**: Every deposit either succeeds on the zone (mint to `to`), bounces back on the zone (mint to `bouncebackAddress`), or is a terminal bounce deposit that must succeed.

5. **Portal bypasses TIP-20 policy for outbound transfers**: The portal bypasses TIP-20 transfer policy checks for outbound transfers only (withdrawal processing, fee payouts). Inbound user deposits still go through normal `transferFrom` with TIP-403 enforcement. The portal enforces policy itself via the bounceback mechanism.

6. **Hash chain integrity**: All deposit types (Regular, Encrypted, BounceBack) include a type discriminator in the hash chain so they can be distinguished and verified by the proof system.

7. **Withdrawal bouncebacks are unchanged**: The existing `processWithdrawal → _enqueueBounceBack → fallbackRecipient` flow continues to work as before, now using `DepositType.BounceBack` for the re-deposit.

8. **Cross-zone failure cascades correctly**: If a cross-zone deposit via `SwapAndDepositRouter` fails at callback time, the withdrawal bounces back to the source zone. If it fails at zone-side processing time, it bounces to the `bouncebackAddress` on the target zone.

## Critical Test Cases

1. **Deposit bounces when recipient is blacklisted on zone**: Deposit succeeds on Tempo, recipient gets blacklisted before zone processing, funds go to `bouncebackAddress` on zone.
2. **Deposit reverts when recipient is blacklisted at deposit time**: No funds move.
3. **Deposit reverts when `bouncebackAddress` is blacklisted at deposit time**: No funds move.
4. **Bounce deposit mint failure reverts zone block**: A `DepositType.BounceBack` deposit where the recipient is blacklisted causes a revert.
5. **Cross-zone transfer failure bounces to source zone**: Via `SwapAndDepositRouter`, if the target zone deposit callback fails, funds return to source zone `fallbackRecipient`.
6. **Encrypted deposit bounces when decrypted recipient is blacklisted on zone**: Decryption succeeds but mint fails → bounce to `bouncebackAddress`.
7. **Portal transfers succeed despite recipient blacklist**: Portal uses `systemTransferFrom`, not `transfer`, so TIP-20 policy does not block portal operations.
8. **`bouncebackAddress` of `address(0)` is rejected on user deposits**: Only the portal itself can create deposits with `address(0)` bounceback.
9. **Encrypted deposit decryption failure bounces to `bouncebackAddress`**: Not `sender`. Especially critical for router-mediated deposits where `sender` is the `SwapAndDepositRouter` contract.
10. **`fallbackRecipient` validated at withdrawal request time**: `ZoneOutbox.requestWithdrawal` rejects withdrawal requests where `fallbackRecipient` is not authorized under TIP-403 mint-recipient policy.
