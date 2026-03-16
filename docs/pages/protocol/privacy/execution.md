# Privacy Zone Execution Environment (Draft)

This document specifies how the EVM execution environment of a privacy zone differs from a standard Tempo zone. These changes are enforced at the execution level (inside the zone's TIP-20 precompile, gas accounting, and EVM configuration), not at the RPC layer. They apply to all code paths — user transactions, sequencer system calls, `eth_call` simulations, and prover re-execution.

For RPC-level access controls (authentication, method filtering, event scoping), see the [Zone RPC Specification](./rpc).

A reference Solidity specification of all TIP-20 modifications is available at [`PrivateZoneToken.sol`](/specs/src/zone/PrivateZoneToken.sol).

## TIP-20 modifications

Privacy zones modify the zone token's TIP-20 precompile in four areas: balance privacy, allowance privacy, fixed gas accounting, and system mint/burn permissions.

### Balance privacy: `balanceOf` access control

On a standard zone (and on Tempo), `balanceOf(address account)` is a public view function — any caller can read any account's balance. On a privacy zone, the function enforces caller restrictions:

- If `msg.sender == account`, the call succeeds and returns the balance.
- If `msg.sender` is the sequencer (as read from `ZoneConfig.sequencer()`), the call succeeds.
- Otherwise, the call reverts with `Unauthorized()`.

This means:

- **User transactions**: A contract calling `balanceOf(someOtherAddress)` will revert. Only self-queries succeed.
- **`eth_call` simulations**: The RPC server sets `from` to the authenticated account, so `balanceOf` only works for the caller's own address. See [RPC spec](./rpc).
- **Sequencer and system calls**: The sequencer retains full read access, which is required for block production, deposit processing, and fee accounting.

**Rationale**: On a public chain, anyone can read any balance. On a privacy zone, balances are private. Enforcing this at the EVM level (not just at the RPC layer) ensures that even on-chain composition cannot leak balances — a contract deployed on the zone cannot be used to read and re-emit another account's balance.

### Allowance privacy: `allowance` access control

The `allowance(address owner, address spender)` function is similarly restricted:

- If `msg.sender == owner` or `msg.sender == spender`, the call succeeds and returns the allowance.
- If `msg.sender` is the sequencer, the call succeeds.
- Otherwise, the call reverts with `Unauthorized()`.

**Rationale**: A non-zero allowance reveals that `owner` has interacted with `spender` — a relationship that should be private on a privacy zone. Restricting reads to the two parties involved preserves standard ERC-20 approval flows (both the owner and the spender can check the allowance) without leaking relationship information to third parties.

**Unchanged views**: `totalSupply()`, `name()`, `symbol()`, `decimals()`, and other non-per-account view functions remain unrestricted.

### Fixed gas: constant transfer cost

All TIP-20 transfer operations on a privacy zone charge a fixed gas cost of **100,000 gas**, regardless of execution-dependent factors:

| Function | Gas cost |
|----------|----------|
| `transfer(to, amount)` | 100,000 |
| `transferFrom(from, to, amount)` | 100,000 |
| `transferWithMemo(to, amount, memo)` | 100,000 |
| `transferFromWithMemo(from, to, amount, memo)` | 100,000 |
| `approve(spender, amount)` | 100,000 |

On a standard EVM chain, gas cost varies depending on whether a transfer writes to a previously empty storage slot (zero → non-zero costs 20,000 gas more than non-zero → non-zero). This difference reveals whether the recipient has previously received tokens — a binary signal about account existence.

By fixing the gas cost:

- All transfer receipts have identical `gasUsed` for the TIP-20 portion, removing the side channel.
- Observers (including the sender, who sees their own receipt) cannot distinguish transfers to new vs. existing accounts.
- The fixed cost of 100,000 gas matches the zone's `FIXED_DEPOSIT_GAS` constant, providing a consistent gas unit across deposits and transfers.

**Implementation**: The zone's TIP-20 precompile always charges exactly 100,000 gas for any transfer-family call, regardless of the actual storage operations required. If the transaction provides less than 100,000 gas to the precompile call, it reverts with out-of-gas. Excess gas beyond 100,000 is returned to the caller as usual.

**Unchanged operations**: System functions (`systemTransferFrom`, `transferFeePreTx`, `transferFeePostTx`) retain their standard gas costs. These are restricted to precompile-only callers where the gas side channel is not exploitable.

### System mint and burn permissions

On Tempo, `mint()` and `burn()` on a TIP-20 require the caller to hold `ISSUER_ROLE`. On a privacy zone, the zone token is a bridged representation — tokens are minted when deposits arrive from Tempo and burned when withdrawals are requested. The zone's system contracts need to perform these operations without holding `ISSUER_ROLE`.

The TIP-20 precompile on a privacy zone extends the mint/burn authorization to include the zone system contracts:

| Operation | Standard TIP-20 access | Privacy zone access |
|-----------|----------------------|-------------------|
| `mint(to, amount)` | `ISSUER_ROLE` only | `ISSUER_ROLE` **or** ZoneInbox (`0x1c...0001`) |
| `burn(from, amount)` | `ISSUER_ROLE` only | `ISSUER_ROLE` **or** ZoneOutbox (`0x1c...0002`) |

Authorization is **operation-specific**: ZoneInbox access applies to `mint` only, and ZoneOutbox access applies to `burn` only. Implementations MUST NOT use a shared "inbox-or-outbox" check for both operations.

**ZoneInbox mints** during deposit processing in `advanceTempo()`:

- Regular deposit: `mint(deposit.to, deposit.amount)` — credits the recipient.
- Encrypted deposit (decryption succeeded): `mint(decrypted.to, deposit.amount)` — credits the decrypted recipient.
- Encrypted deposit (decryption failed): `mint(deposit.sender, deposit.amount)` — refunds the sender.

**ZoneOutbox burns** during withdrawal requests in `requestWithdrawal()`:

- The user approves the ZoneOutbox to spend `amount + fee`.
- ZoneOutbox calls `transferFrom(user, self, amount + fee)`, then `burn(self, amount + fee)`.
- The burned tokens are released on Tempo when the sequencer processes the withdrawal.

**Gas costs**: `mint` and `burn` retain standard variable gas costs (not the fixed 100,000). These functions are only called by system contracts during sequencer operations, so there is no user-exploitable gas side channel.

**`ISSUER_ROLE` is preserved** for forward compatibility but is not expected to be granted to any external party on a zone — the zone token supply is entirely controlled by the bridge mechanism.

## EVM restrictions

### Contract creation restricted

Privacy zones restrict the `CREATE` and `CREATE2` opcodes to a set of **whitelisted deployer addresses**. Any `CREATE` or `CREATE2` executed by a non-whitelisted address reverts. By default the whitelist is empty — the zone launches with no user-deployable contracts, only the fixed predeploys.

**Whitelist management**: The sequencer manages the deployer whitelist via `ZoneConfig`:

```solidity
/// @notice Emitted when a deployer is added or removed.
event DeployerWhitelistUpdated(address indexed deployer, bool allowed);

/// @notice Returns true if the address is allowed to execute CREATE/CREATE2.
function isWhitelistedDeployer(address deployer) external view returns (bool);

/// @notice Add or remove a deployer. Callable only by the sequencer.
function setWhitelistedDeployer(address deployer, bool allowed) external;
```

`setWhitelistedDeployer` is a sequencer-only system transaction — it reverts if `msg.sender != sequencer()`. The whitelist is stored in `ZoneConfig` contract state and checked by the EVM at opcode execution time.

**EVM enforcement**: When the EVM encounters `CREATE` or `CREATE2`, it checks `ZoneConfig.isWhitelistedDeployer(executingAddress)` where `executingAddress` is the account whose code is running. If the check fails, the opcode reverts. For a direct deployment transaction (a tx with `to` = null), the executing address is the sender EOA. For a factory call, it is the factory contract. This means the same whitelist controls both EOA-initiated deployments and factory-initiated deployments.

**Contracts deployed by whitelisted deployers are not themselves whitelisted** — they cannot deploy further contracts unless the sequencer explicitly adds them.

**Bootstrapping a factory**: The sequencer whitelists their own EOA, deploys the factory contract via a direct deployment transaction, whitelists the factory address, and removes their own EOA. After this sequence only the factory can create new contracts.

**Rationale**: While per-function access control (`balanceOf`, `allowance`) blocks direct balance reads by third-party contracts, arbitrary deployment still poses risks:

1. **Gas side channels**: Standard EVM gas costs leak information. For example, a TIP-20 transfer to a new account (zero → non-zero storage) costs more than a transfer to an existing account. The zone's [fixed gas cost](#fixed-gas-constant-transfer-cost) mitigates this for the TIP-20 precompile, but user-deployed contracts with their own storage would reintroduce the same class of side channel — any storage write whose cost depends on the target's prior state reveals one bit of information per call.
2. **No automatic privacy**: Contracts deployed on a privacy zone are not automatically private. Keeping user data confidential requires deliberate design — scoped view functions, fixed gas costs, and careful event filtering. Unrestricted deployment would create a false expectation that any contract inherits the zone's privacy properties, when in practice most contracts would leak information through public storage, events, or gas patterns.

The whitelist ensures that only contracts with audited privacy behavior are deployed, and sets a clear expectation that each whitelisted contract has been reviewed for information leakage.

## Interaction with RPC

These execution-level changes are the first line of defense. The [RPC specification](./rpc) adds a second layer of access control (authentication, method restrictions, event filtering). Both layers are required:

- **Execution alone is insufficient**: Without RPC restrictions, a caller could use `eth_getStorageAt` to read TIP-20 balance mapping slots directly, bypassing the `balanceOf` access control entirely.
- **RPC alone is insufficient**: Without execution-level changes, a caller could deploy or call a contract via `eth_call` that reads another account's balance and returns it, bypassing RPC-level filtering.

The two layers are complementary: execution-level changes protect against in-EVM information leaks, and RPC-level changes protect against raw state inspection.
