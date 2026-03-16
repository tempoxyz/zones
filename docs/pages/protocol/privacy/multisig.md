# Deploying a Gnosis Safe on a Privacy Zone

This document describes how to deploy and use a Gnosis Safe multisig wallet on a privacy zone. It covers the contracts involved, the sequencer setup steps, user-facing creation flow, and how the Safe interacts with the zone's privacy features.

## Prerequisites

- The zone's [deployer whitelist](./execution#contract-creation-restricted) mechanism is active.
- The sequencer has access to `ZoneConfig.setWhitelistedDeployer`.
- The zone supports `DELEGATECALL` (required by Safe's minimal proxy pattern).
- [Contract read delegation](./rpc#contract-read-delegation) is enabled on the RPC.

## Contracts

A Gnosis Safe deployment requires three contracts:

| Contract | Purpose | Deploys children? |
|----------|---------|-------------------|
| **Safe singleton** | Master copy containing all Safe logic (`execTransaction`, `addOwnerWithThreshold`, etc.). Never called directly — used as a `DELEGATECALL` target. | No |
| **SafeProxyFactory** | Creates new Safe instances via `CREATE2`. Each instance is a minimal proxy (EIP-1167) that delegates to the singleton. | Yes |
| **CompatibilityFallbackHandler** | Default fallback handler for view functions (`isValidSignature`, `getMessageHash`, etc.). Set during Safe initialization. | No |

Only the **SafeProxyFactory** needs to be whitelisted as a deployer, because it is the only contract that executes `CREATE2`. The singleton and fallback handler are passive — they are deployed once and referenced by address.

## Sequencer setup

The sequencer performs these steps as system transactions. Each step is a separate transaction.

### 1. Whitelist the sequencer EOA

```
ZoneConfig.setWhitelistedDeployer(sequencer, true)
```

### 2. Deploy the Safe contracts

Three direct deployment transactions (tx with `to` = null):

1. **Safe singleton** — deploy the `Safe` master copy. Record the deployed address `SINGLETON`.
2. **CompatibilityFallbackHandler** — deploy the fallback handler. Record the address `FALLBACK_HANDLER`.
3. **SafeProxyFactory** — deploy the proxy factory. Record the address `FACTORY`.

The sequencer can use `CREATE2` with fixed salts to make these addresses deterministic and reproducible across zones.

### 3. Whitelist the factory

```
ZoneConfig.setWhitelistedDeployer(FACTORY, true)
```

### 4. Remove the sequencer EOA from the whitelist

```
ZoneConfig.setWhitelistedDeployer(sequencer, false)
```

After this step, only the factory can create new contracts. The sequencer can no longer deploy arbitrary code.

### 5. Publish the addresses

The sequencer publishes `SINGLETON`, `FACTORY`, and `FALLBACK_HANDLER` as part of the zone's public configuration so that users can construct Safe creation transactions.

## User flow: creating a Safe

A user creates a new Safe by sending a transaction to the SafeProxyFactory. No deployer whitelist entry is needed for the user — the factory is whitelisted, and it executes the `CREATE2`.

### 1. Prepare the initializer

The initializer is an ABI-encoded call to `Safe.setup()`:

```solidity
bytes memory initializer = abi.encodeCall(Safe.setup, (
    owners,           // address[] — initial owner addresses
    threshold,        // uint256 — required signatures (k of n)
    address(0),       // address — optional delegate call target (none)
    "",               // bytes — optional delegate call data (none)
    FALLBACK_HANDLER, // address — fallback handler
    address(0),       // address — payment token (none)
    0,                // uint256 — payment amount (none)
    payable(address(0)) // address — payment receiver (none)
));
```

### 2. Compute the Safe address

The user can deterministically compute their Safe address before deployment:

```
address safe = keccak256(
    0xff,
    FACTORY,
    salt,
    keccak256(proxyCreationCode ++ SINGLETON)
)
```

where `salt` is `keccak256(keccak256(initializer) ++ userSalt)` per the SafeProxyFactory implementation. The user picks `userSalt` (typically a nonce).

### 3. Send the creation transaction

```solidity
SafeProxyFactory(FACTORY).createProxyWithNonce(SINGLETON, initializer, userSalt)
```

The factory calls `CREATE2` to deploy a minimal proxy pointing to `SINGLETON`, then calls `setup()` on the new proxy with the provided initializer. The Safe is now live.

### 4. Fund the Safe

Transfer zone tokens to the computed Safe address. Since the address is deterministic, the user can fund it before or after deployment.

## Using the Safe

### Reading Safe state (RPC)

Each Safe owner authenticates to the zone RPC with their own key. The RPC server calls `getOwners()` on the Safe (via [contract read delegation](./rpc#contract-read-delegation)) and grants read access if the caller is in the owner list. This allows owners to:

- Query the Safe's token balance via `eth_getBalance`.
- Call view functions (`getOwners()`, `getThreshold()`, `nonce()`) via `eth_call`.
- Retrieve Safe-related events via `eth_getLogs`.

Non-owners see dummy values for the Safe address, same as any other account they don't control.

### Executing transactions

Safe transactions require k-of-n owner signatures. The flow is:

1. **Propose** — one owner constructs the `execTransaction` payload (target, value, data, operation, signatures).
2. **Collect signatures** — owners sign the Safe transaction hash off-chain. Coordination happens outside the zone (e.g., via a Safe transaction service or direct messaging).
3. **Submit** — any owner submits the fully-signed `execTransaction` call via `eth_sendRawTransaction`. The Safe contract verifies the signatures on-chain and executes the inner transaction.

The zone RPC does not enforce the multisig threshold — that is the Safe contract's responsibility. The RPC only controls _read_ access.

### Owner management

Owners are managed through Safe's standard functions, all executed via `execTransaction` with the required signatures:

- `addOwnerWithThreshold(address owner, uint256 threshold)`
- `removeOwner(address prevOwner, address owner, uint256 threshold)`
- `swapOwner(address prevOwner, address oldOwner, address newOwner)`
- `changeThreshold(uint256 threshold)`

When owners change, the RPC server's [cached `getOwners()` result](./rpc#contract-read-delegation) is invalidated on the next block import. Removed owners lose read access promptly.

## Privacy considerations

- **Owner set visibility**: The Safe's `getOwners()` is a view function callable by anyone through `eth_call`. However, the zone's [RPC scoping](./rpc) restricts `eth_call` — only authenticated owners (via read delegation) can call view functions on the Safe. External observers cannot enumerate the owner set.
- **Transaction privacy**: Safe `execTransaction` calls are submitted as regular zone transactions. The zone's privacy model applies: transaction contents are visible only to the sender (the submitting owner) and the sequencer. Other owners see the _effects_ (balance changes, event logs) through their delegated read access, but not the raw transaction.
- **Signature coordination**: Owners must exchange signatures off-chain before submission. This coordination channel is outside the zone's scope. Owners should use an encrypted channel to avoid leaking transaction intent.
- **`ProxyCreation` events**: The SafeProxyFactory emits a `ProxyCreation(proxy, singleton)` event on deployment. The zone's [event filtering](./rpc#event-filtering) scopes events to accounts the caller is associated with. Since the Safe hasn't been set up yet when the event fires, the event is visible to the transaction sender. After setup, owners gain event access via read delegation.

## Limitations

- **No modules or guards**: The zone does not restrict Safe modules or guards at the protocol level, but enabling them requires careful review — a module could bypass privacy protections if it interacts with external contracts. Sequencers SHOULD document which Safe configurations are supported.
- **No contract-to-contract Safe creation**: The factory is whitelisted, but a contract calling the factory would need the factory to be reachable via a regular call. This works — the restriction is on who executes `CREATE2`, which is always the factory.
- **Single factory**: The zone supports one SafeProxyFactory instance. If a new Safe version is released, the sequencer deploys a new singleton and factory, whitelists the new factory, and optionally removes the old one.
