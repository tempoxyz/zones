# Privacy Zone Execution Environment (Draft)

This document specifies how a zone executes transactions once funds are inside the zone. It covers fee tokens, fee accounting, TIP-20 behavior, token management, and the EVM-level restrictions that make zones private by default.

For the end-to-end zone flow, see the [overview](https://github.com/tempoxyz/zones/blob/docs/zones-specs-entrypoint/docs/specs/privacy/overview.md). For proof construction, see the [Zone Prover Design](https://github.com/tempoxyz/zones/blob/docs/zones-specs-entrypoint/docs/specs/privacy/prover-design.md). For RPC-layer privacy controls, see the [Zone RPC Specification](https://github.com/tempoxyz/zones/blob/docs/zones-specs-entrypoint/docs/specs/privacy/rpc.md).

A zone still executes an EVM-like state transition, but it changes a few core rules:

- Gas can be paid in any enabled zone token.
- Deposits and withdrawals have explicit processing fees.
- TIP-20 reads and gas costs are adjusted to preserve privacy.
- Token mint and burn permissions belong to zone system contracts, not Tempo issuers.
- Contract creation is currently disabled.

A reference Solidity specification of the TIP-20 changes is available at [`PrivateZoneToken.sol`](/specs/src/zone/PrivateZoneToken.sol).

## Fee model

### Fee tokens

Zones reuse Tempo's fee units and transaction shape. Every transaction includes a `feeToken` field selecting which enabled TIP-20 token will pay gas.

Any enabled TIP-20 token with USD currency is valid for gas payment. The sequencer is required to accept all enabled tokens directly, so the zone does not need a fee AMM to convert between gas assets.

### Deposit fees

Deposits charge a fixed processing fee in the same token being deposited:

```text
deposit fee = FIXED_DEPOSIT_GAS * zoneGasRate
```

`FIXED_DEPOSIT_GAS` is fixed at `100,000` gas. The sequencer publishes `zoneGasRate` via `ZonePortal.setZoneGasRate()`.

The fee is deducted on Tempo before the deposit enters the queue:

- The user deposits `amount`.
- The portal transfers `amount` into escrow.
- The portal pays the fee portion to the sequencer.
- The deposit queue stores the net amount, `amount - fee`.
- `ZoneInbox` later mints that net amount on the zone.

This keeps deposit pricing simple while still letting the sequencer update the token-denominated gas rate over time.

### Withdrawal fees

Withdrawals charge a processing fee in the same token being withdrawn:

```text
withdrawal fee = gasLimit * tempoGasRate
```

The user chooses `gasLimit` to cover Tempo-side processing and any optional callback execution. The sequencer publishes `tempoGasRate` via `ZoneOutbox.setTempoGasRate()`.

The withdrawal request burns `amount + fee` on the zone:

- `amount` is the value the user wants delivered on Tempo.
- `fee` compensates the sequencer for Tempo-side execution.
- On success, `amount` goes to the recipient and `fee` goes to the sequencer.
- On failure, the withdrawal bounces back to the zone `fallbackRecipient`, but the fee is still kept by the sequencer because the Tempo-side work was still performed.

## TIP-20 modifications

Privacy zones modify the zone token's TIP-20 precompile in four areas: balance privacy, allowance privacy, fixed gas accounting, and system mint/burn permissions.

### Balance privacy: `balanceOf` access control

On a standard zone, `balanceOf(address account)` is a public view. On a privacy zone, the function enforces caller restrictions:

- If `msg.sender == account`, the call succeeds and returns the balance.
- If `msg.sender` is the sequencer (as read from `ZoneConfig.sequencer()`), the call succeeds.
- Otherwise, the call reverts with `Unauthorized()`.

This means:

- User transactions cannot read another account's balance through a contract call.
- `eth_call` simulations only work for the authenticated caller's own address when routed through the private RPC.
- Sequencer and system calls still retain the access needed for block production, deposit processing, and fee accounting.

The important design point is that this restriction is enforced inside execution, not just in the RPC server. A contract deployed on the zone must not be able to read and re-emit another user's balance.

### Allowance privacy: `allowance` access control

`allowance(address owner, address spender)` is similarly restricted:

- If `msg.sender == owner` or `msg.sender == spender`, the call succeeds and returns the allowance.
- If `msg.sender` is the sequencer, the call succeeds.
- Otherwise, the call reverts with `Unauthorized()`.

This preserves standard approval flows while preventing third parties from learning that two accounts have an allowance relationship.

Unchanged non-account-scoped views such as `totalSupply()`, `name()`, `symbol()`, and `decimals()` remain unrestricted.

### Fixed gas: constant transfer cost

All user-facing TIP-20 transfer and approval operations charge a fixed gas cost of `100,000`, regardless of the underlying storage writes:

| Function | Gas cost |
|----------|----------|
| `transfer(to, amount)` | 100,000 |
| `transferFrom(from, to, amount)` | 100,000 |
| `transferWithMemo(to, amount, memo)` | 100,000 |
| `transferFromWithMemo(from, to, amount, memo)` | 100,000 |
| `approve(spender, amount)` | 100,000 |

On a normal EVM chain, gas differs depending on whether storage moves from zero to non-zero or non-zero to non-zero. That leaks whether the recipient already had a balance, which is a privacy side channel.

By fixing the gas cost:

- The TIP-20 portion of every transfer receipt looks the same.
- Senders cannot infer whether the recipient was a new or existing account.
- Deposit accounting and transfer accounting share the same `100,000` gas unit.

System functions such as `systemTransferFrom`, `transferFeePreTx`, and `transferFeePostTx` keep their standard variable gas costs. Those functions are restricted to system callers, so this side channel does not apply.

### System mint and burn permissions

On Tempo, `mint()` and `burn()` require issuer privileges. On a zone, tokens are bridged representations, so mint and burn authority belongs to the zone system contracts instead:

| Operation | Standard TIP-20 (Tempo) | Zone access |
|-----------|-------------------------|-------------|
| `mint(to, amount)` | `ISSUER_ROLE` only | `ZoneInbox` only |
| `burn(from, amount)` | `ISSUER_ROLE` only | `ZoneOutbox` only |

Authorization is operation-specific: `ZoneInbox` may mint but not burn, and `ZoneOutbox` may burn but not mint.

`ZoneInbox` mints during deposit processing:

- Regular deposit: mint to the deposit recipient.
- Encrypted deposit with successful decryption: mint to the decrypted recipient.
- Encrypted deposit with failed decryption: mint to the depositor's zone address so the deposit cannot block progress.

`ZoneOutbox` burns during withdrawal requests:

- The user approves the outbox for `amount + fee`.
- The outbox transfers that amount to itself.
- The outbox burns the full `amount + fee`.
- Tempo later releases the escrowed assets when the withdrawal is processed.

`mint` and `burn` keep their normal variable gas costs. These calls only occur in sequencer-controlled system flows, so a user-visible side channel does not arise.

## Token management

The sequencer manages which TIP-20 tokens exist on the zone:

| Function | Behavior |
|----------|----------|
| `enableToken(token)` | Enables a new TIP-20 for bridging and gas payment. Irreversible. |
| `pauseDeposits(token)` | Stops new deposits for that token. Withdrawals continue to work. |
| `resumeDeposits(token)` | Re-enables deposits for a previously paused token. |

The portal tracks two independent properties per token:

- `enabled`: append-only, once set it can never be removed.
- `depositsActive`: a sequencer-controlled toggle for new deposits.

This separation preserves the non-custodial withdrawal guarantee: once a token is enabled, the sequencer can pause new deposits but cannot strand existing balances by disabling withdrawals.

Enabled TIP-20 tokens use the same address on the zone as on Tempo. There is no zone-side token factory. `ZoneInbox` mints bridged balances and `ZoneOutbox` burns them again when users leave the zone.

## EVM restrictions

### Contract creation disabled

Privacy zones currently disable the `CREATE` and `CREATE2` opcodes. The zone runs a fixed set of predeploys and token contracts, and user-deployed contracts are not yet supported. Any transaction or call that attempts contract creation reverts.

This restriction removes a large class of privacy footguns. In particular, it prevents users from deploying arbitrary helper contracts that try to read restricted state and re-emit it.

## Interaction with RPC

These execution-level rules are only one layer of the privacy model. The [RPC specification](https://github.com/tempoxyz/zones/blob/docs/zones-specs-entrypoint/docs/specs/privacy/rpc.md) adds a second layer of access control:

- Execution-level changes prevent information leaks from inside the EVM.
- RPC-level changes prevent raw state inspection, unrestricted transaction lookup, and unscoped event access.

Neither layer is sufficient on its own. Without RPC restrictions, callers could use raw state queries such as `eth_getStorageAt` to bypass `balanceOf`. Without execution-level restrictions, a caller could use `eth_call` or other EVM execution paths to retrieve another account's balance indirectly.
