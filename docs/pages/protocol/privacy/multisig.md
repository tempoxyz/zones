# Deploying a Gnosis Safe on a Privacy Zone

This document describes how to deploy and use a multisig wallet on a privacy zone. The zone uses a locked-down variant of Gnosis Safe ([`PrivateZoneSafe`](/specs/src/zone/PrivateZoneSafe.sol)) and a minimal factory ([`PrivateZoneSafeFactory`](/specs/src/zone/PrivateZoneSafeFactory.sol)) designed for the zone's privacy model.

## Prerequisites

- The zone's [deployer whitelist](./execution#contract-creation-restricted) mechanism is active.
- The sequencer has access to `ZoneConfig.setWhitelistedDeployer`.
- [Contract read delegation](./rpc#contract-read-delegation) is enabled on the RPC.

## Contracts

| Contract | Reference spec | Purpose | Whitelisted? |
|----------|---------------|---------|--------------|
| **PrivateZoneSafe** | [`PrivateZoneSafe.sol`](/specs/src/zone/PrivateZoneSafe.sol) | Singleton containing all Safe logic. Proxies delegate to it. | No |
| **PrivateZoneSafeFactory** | [`PrivateZoneSafeFactory.sol`](/specs/src/zone/PrivateZoneSafeFactory.sol) | Deploys Safe proxies via `CREATE2` and calls `setup()`. | **Yes** |
| **CompatibilityFallbackHandler** | (standard Gnosis Safe) | Fallback handler for EIP-1271 and other view functions. Set at factory construction. | No |

Only the factory needs to be whitelisted as a deployer.

### Differences from standard Gnosis Safe

The `PrivateZoneSafe` singleton removes features that are unnecessary or dangerous on a privacy zone:

| Feature | Standard Safe | PrivateZoneSafe | Reason |
|---------|--------------|-----------------|--------|
| Modules | `enableModule`, `execTransactionFromModule`, etc. | Removed | Modules bypass the multisig threshold and could exfiltrate data autonomously |
| Guards | `setGuard` | Removed | Guards add arbitrary pre/post hooks that could interact with external state |
| DELEGATECALL | `operation = 0` or `1` | `operation = 0` only | DELEGATECALL runs arbitrary code in the Safe's storage context |
| `getStorageAt` | Reads arbitrary storage slots | Removed | Reduces on-chain attack surface for cross-contract probing |
| `setFallbackHandler` | Changeable post-setup | Removed | Prevents post-initialization handler changes that could leak information |
| Gas refunds | `gasPrice`, `gasToken`, `refundReceiver` | Removed | Refund logic uses variable gas; zone's fixed-fee model makes refunds unnecessary |
| `execTransaction` | 10 parameters | 4 parameters (`to`, `value`, `data`, `signatures`) | Simplified — no operation, no refund fields |

The factory is also simplified: `PrivateZoneSafeFactory` bakes in the singleton and fallback handler at construction, so `createProxy` takes only `owners`, `threshold`, and `userSalt`. A `computeAddress` view function lets users derive their Safe's address before deployment.

### Public view functions

The Safe's view functions (`getOwners`, `isOwner`, `getThreshold`, `nonce`) are public — any authenticated user can call them via `eth_call`. This is an accepted trade-off: `getOwners()` must be public for the RPC's [contract read delegation](./rpc#contract-read-delegation) mechanism to work, and the remaining views expose no information beyond what `getOwners()` already reveals.

## Sequencer setup

The sequencer performs these steps as system transactions.

### 1. Whitelist the sequencer EOA

```
ZoneConfig.setWhitelistedDeployer(sequencer, true)
```

### 2. Deploy the contracts

Three direct deployment transactions (tx with `to` = null):

1. **PrivateZoneSafe** singleton — record the deployed address `SINGLETON`.
2. **CompatibilityFallbackHandler** — record the address `FALLBACK_HANDLER`.
3. **PrivateZoneSafeFactory** — constructed with `(SINGLETON, FALLBACK_HANDLER)`. Record the address `FACTORY`.

The sequencer can use `CREATE2` with fixed salts to make these addresses deterministic and reproducible across zones.

### 3. Whitelist the factory

```
ZoneConfig.setWhitelistedDeployer(FACTORY, true)
```

### 4. Remove the sequencer EOA from the whitelist

```
ZoneConfig.setWhitelistedDeployer(sequencer, false)
```

After this step, only the factory can create new contracts.

### 5. Publish the addresses

The sequencer publishes `FACTORY` as part of the zone's public configuration. The singleton and fallback handler addresses are readable from the factory (`factory.singleton()`, `factory.fallbackHandler()`).

## User flow: creating a Safe

A user creates a new Safe by calling the factory. No deployer whitelist entry is needed for the user — the factory is whitelisted, and it executes the `CREATE2`.

### 1. Compute the Safe address (optional)

The factory provides a view function for deterministic address computation:

```solidity
address safe = factory.computeAddress(owners, threshold, userSalt);
```

The user can fund this address before deployment if desired.

### 2. Deploy the Safe

```solidity
address safe = factory.createProxy(owners, threshold, userSalt);
```

The factory:
1. Encodes `PrivateZoneSafe.setup(owners, threshold, fallbackHandler)` as the initializer.
2. Computes `salt = keccak256(abi.encode(keccak256(initializer), userSalt))`.
3. Deploys a `PrivateZoneSafeProxy` via `CREATE2`.
4. Calls `setup()` on the new proxy.
5. Emits `ProxyCreation(proxy, singleton)`.

### 3. Fund the Safe

Transfer zone tokens to the Safe address.

## Using the Safe

### Reading Safe state (RPC)

Each Safe owner authenticates to the zone RPC with their own key. The RPC server calls `getOwners()` on the Safe (via [contract read delegation](./rpc#contract-read-delegation)) and grants read access if the caller is in the owner list. This allows owners to:

- Query the Safe's token balance via `eth_getBalance`.
- Call view functions (`getOwners()`, `getThreshold()`, `nonce()`) via `eth_call`.
- Retrieve Safe-related events via `eth_getLogs`.

Non-owners see dummy values for the Safe address, same as any other account they don't control.

### Executing transactions

The `PrivateZoneSafe` has a simplified `execTransaction` with four parameters:

```solidity
function execTransaction(
    address to,
    uint256 value,
    bytes calldata data,
    bytes calldata signatures
) external returns (bool success);
```

The flow:

1. **Propose** — one owner constructs the transaction payload and computes the Safe transaction hash using `getTransactionHash(to, value, data, nonce)`.
2. **Collect signatures** — owners sign the EIP-712 hash off-chain. Signatures must be sorted by signer address in ascending order. Coordination happens outside the zone (e.g., via encrypted messaging).
3. **Submit** — any owner submits the fully-signed `execTransaction` call via `eth_sendRawTransaction`.

The contract verifies the signatures, increments the nonce, and executes the inner call. Only `CALL` operations are supported — `DELEGATECALL` is disabled.

### Approving hashes on-chain

As an alternative to off-chain ECDSA signatures, owners can pre-approve a transaction hash on-chain:

```solidity
safe.approveHash(txHash);
```

When constructing the signatures array, a pre-approved hash is encoded with `v = 1` and `r = ownerAddress`. This is useful when an owner cannot produce an off-chain signature (e.g., a hardware wallet integration that prefers on-chain approval).

### Owner management

Owners are managed through the Safe's own functions, executed via `execTransaction` (self-call):

- `addOwnerWithThreshold(address owner, uint256 threshold)`
- `removeOwner(address prevOwner, address owner, uint256 threshold)`
- `swapOwner(address prevOwner, address oldOwner, address newOwner)`
- `changeThreshold(uint256 threshold)`

When owners change, the RPC server's [cached `getOwners()` result](./rpc#contract-read-delegation) is invalidated on the next block import. Removed owners lose read access promptly.

## Privacy analysis

### What the RPC protects

- **Balance and nonce**: `eth_getBalance` and `eth_getTransactionCount` for the Safe address return real values only for owners (via read delegation). Non-owners get `0x0`.
- **Events** (`SafeSetup`, `AddedOwner`, `RemovedOwner`, `ChangedThreshold`, `ExecutionSuccess`, `ExecutionFailure`): Scoped by the RPC's [event filtering](./rpc#event-filtering) to the Safe's owners. Non-owners do not see Safe events.
- **Transaction receipts**: Only the submitting owner can retrieve the `execTransaction` receipt via `eth_getTransactionReceipt`.
- **`ProxyCreation` events**: Emitted by the factory during deployment, before the Safe is initialized. Visible to the creation transaction sender. After `setup()`, owners gain event access via delegation.

### Public view functions

Any authenticated user can call the Safe's view functions via `eth_call`:

| Function | What it reveals |
|----------|----------------|
| `getOwners()` | Full owner address list |
| `isOwner(address)` | Whether a specific address is an owner |
| `getThreshold()` | Required number of signatures |
| `nonce()` | Number of executed transactions |
| `domainSeparator()` | EIP-712 domain (derived from chain ID and Safe address — no private data) |
| `getTransactionHash(...)` | Transaction hash for given parameters (pure computation — no private data) |
| `approvedHashes(owner, hash)` | Whether a specific owner pre-approved a specific hash |

This is an accepted trade-off. `getOwners()` must be public for the RPC's contract read delegation to work. The other functions expose no information beyond what `getOwners()` reveals, with two exceptions:

- **`nonce()`** reveals the Safe's total transaction count — a measure of activity. This is minor; the same information could be inferred by watching `ExecutionSuccess` events (which are RPC-scoped), but `nonce()` makes it available directly.
- **`approvedHashes(owner, hash)`** reveals whether a specific owner pre-approved a specific hash. An attacker would need to know both the owner address (available via `getOwners()`) and the exact transaction hash (which requires knowing the transaction parameters). This is acceptable because knowing the transaction parameters already implies access to the transaction details.

### What the sequencer sees

The sequencer processes all transactions and has full state access:

- **Owner addresses**: Via `SafeSetup`, `AddedOwner`, and `RemovedOwner` events.
- **Signer identities**: The `signatures` field in `execTransaction` contains ECDSA signatures from which signer addresses can be recovered. The sequencer learns which specific owners signed each transaction.
- **Transaction contents**: The inner `to`, `value`, and `data` of every Safe transaction are visible in the `execTransaction` calldata.

This is consistent with the zone's general trust model — the sequencer is trusted with transaction contents.

### On-chain cross-contract probing

One Safe's `execTransaction` could call another Safe's view functions. The information leakage from this is limited:

- `execTransaction` discards the inner call's return data. The caller observes only success/failure.
- View functions return values without reverting, so the success signal is constant regardless of the return value.
- No helper contracts can be deployed to relay return data (deployer whitelist prevents this).

**Residual risk**: Gas measurement of the inner call could serve as a side channel (e.g., `isOwner` iterates the owner linked list, and gas cost varies by list position). This requires a coordinated multisig action and the signal is noisy. Mitigation via constant-gas view functions is possible but not required for initial deployment.

### Signature coordination

Owners must exchange signatures off-chain before submission. This coordination channel is outside the zone's scope. Owners SHOULD use an end-to-end encrypted channel to avoid leaking transaction intent and signer identities to third parties.

## Upgrading

If a new `PrivateZoneSafe` version is needed (e.g., after a protocol upgrade), the sequencer:

1. Whitelists their EOA.
2. Deploys a new singleton and factory.
3. Whitelists the new factory.
4. Removes their EOA from the whitelist.
5. Optionally removes the old factory from the whitelist (existing Safes continue to work — they delegate to the old singleton, which remains deployed).

Existing Safes are not affected. New Safes are created against the new singleton.
