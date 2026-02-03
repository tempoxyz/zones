# L1 State Subscriptions

This document describes the L1 state subscription model for Tempo Zones, an alternative to transaction-level state declarations.

## Overview

Instead of requiring users to declare L1 state in each transaction, the subscription model allows:

1. **Upfront subscription** to specific L1 state slots (account, slot) pairs
2. **Automatic subscription** for TIP-403 policy state (zone token transfers)
3. **Monthly subscription fees** paid to the sequencer
4. **Real-time state sync** by the sequencer
5. **Complete replay capability** via L1 state access logs

## Architecture

### Components

#### 1. L1StateSubscriptionManager (Predeploy at 0x1c00000000000000000000000000000000000001)

Manages subscriptions to L1 state slots:

- **Subscribe**: Users call `subscribe(account, slot, months)` and pay `monthlySubscriptionFee * months` in zone tokens
- **Auto-subscription**: TIP-403 policy state for the zone token is automatically subscribed at genesis (permanent)
- **Fee management**: Sequencer sets `monthlySubscriptionFee` to cover L1 sync costs
- **Expiry tracking**: Subscriptions expire after the paid period unless extended

**Key storage:**
```solidity
mapping(bytes32 => uint64) public subscriptionExpiry;  // keccak256(abi.encode(account, slot)) => expiryTimestamp
uint128 public monthlySubscriptionFee;                 // Set by sequencer
```

**Automatic TIP-403 subscriptions:**
- Zone token's `transferPolicyId` (permanent)
- TIP-403 `_policyData[transferPolicyId]` (permanent, added by sequencer)
- Dynamic: `policySet[transferPolicyId][address]` entries (added by sequencer as needed)

#### 2. TempoState (Predeploy at 0x1c00000000000000000000000000000000000000)

Enhanced to validate subscriptions and log accesses:

- **finalizeTempo**: Sequencer submits Tempo headers to advance zone's L1 view
- **readTempoStorageSlot**:
  1. Validates subscription via `subscriptionManager.isSubscribed(account, slot)`
  2. Reads value from sequencer's synced L1 state cache (precompile)
  3. Logs access to current block's `L1StateAccessLog` (precompile)
  4. Returns value
- **readTempoStorageSlots**: Batch version of above

#### 3. L1StateAccessLog (per zone block)

Records all L1 state accesses for replay/reconstruction:

```solidity
struct L1StateAccessEntry {
    address account;  // L1 contract address
    bytes32 slot;     // Storage slot
    bytes32 value;    // Value at time of access
}

struct L1StateAccessLog {
    L1StateAccessEntry[] accesses;  // All L1 reads in this block
}
```

- **Committed in batch proofs**: Sequencer includes `L1StateAccessLog` hash in batch submission
- **Available for replay**: Anyone can reconstruct zone state from genesis using these logs
- **Prevents disputes**: Complete audit trail of all L1 dependencies

## Workflow

### User subscribing to custom L1 state

1. User approves L1StateSubscriptionManager to spend zone tokens
2. User calls `subscriptionManager.subscribe(account, slot, months)`
3. Contract transfers `monthlySubscriptionFee * months` zone tokens to sequencer
4. Subscription becomes active immediately
5. User can now call contracts that read `(account, slot)` via TempoState

### TIP-20 transfers (automatic subscription)

1. At genesis: Zone token's `transferPolicyId` slot is auto-subscribed (permanent)
2. After genesis: Sequencer reads `transferPolicyId` from L1 and calls `autoSubscribePolicyState(transferPolicyId)`
3. Policy data subscription becomes permanent
4. When sequencer sees new addresses in TIP-20 transfers, it auto-subscribes their policy set entries
5. Users can make TIP-20 transfers without manual subscription

### Sequencer's L1 sync responsibilities

1. **Maintain L1 state cache**: Track all subscribed slots in real-time by monitoring L1
2. **Update on Tempo finalization**: When calling `finalizeTempo()`, update cache with latest L1 values
3. **Serve reads via precompile**: Implement precompile that:
   - Validates subscription
   - Returns value from cache
   - Logs to current block's `L1StateAccessLog`
4. **Include logs in proofs**: Commit to `L1StateAccessLog` hash in batch submissions

### Replay and reconstruction

Anyone can reconstruct zone state from genesis:

1. Start with genesis state
2. For each zone block:
   - Read `L1StateAccessLog` from block data
   - Replay all transactions with L1 state reads satisfied by the log
   - Verify state transition matches claimed block hash

## Comparison to State Declarations

| Aspect | State Declarations (type 0x7A) | Subscriptions |
|--------|-------------------------------|---------------|
| **User friction** | High - declare per tx | Low - subscribe once |
| **Wallet support** | Requires new tx type | Works with standard txs |
| **Sequencer cost** | Low - just validate | Higher - maintain sync |
| **Replay** | Embedded in tx | Separate access log |
| **Flexibility** | Any slot, any time | Only subscribed slots |
| **Economics** | Gas per declaration | Monthly subscription fee |

## Security Considerations

### Subscription expiry

- Contracts must handle `SubscriptionExpired` errors gracefully
- Users should monitor expiry and extend before it lapses
- Auto-subscribed TIP-403 slots are permanent (never expire)

### Sequencer trust assumptions

- Sequencer must honestly sync L1 state
- Sequencer could censor by not subscribing policy set entries
- Validity proofs verify all reads are subscribed and logged correctly
- L1StateAccessLog provides complete audit trail

### Replay attacks

- L1StateAccessLog records value **at time of access**, not current value
- Replaying with stale values will fail state transition verification
- Proofs ensure logs match actual L1 state at the finalized Tempo block

## Economics

### Subscription pricing

Sequencer sets `monthlySubscriptionFee` to cover:
- L1 monitoring costs (events, state polling)
- Storage costs for cache
- Proof generation overhead for access logs

Suggested model:
```
baseFee = cost to monitor one slot for 30 days
fee per slot = baseFee * riskMultiplier
```

Where `riskMultiplier` accounts for:
- Volatility of the slot (frequent updates = higher risk)
- Number of subscribers (amortization)

### Fee distribution

All subscription fees are **transferred to the sequencer**:
- Direct compensation for L1 state sync infrastructure costs
- Aligns incentives: sequencer earns more as zone usage grows
- Complements withdrawal processing fees for sequencer revenue

## Implementation Notes

### Precompile requirements

The zone node must implement a precompile for `TempoState.readTempoStorageSlot(account, slot)`:

```rust
fn read_tempo_storage_slot(account: Address, slot: H256) -> Result<H256, Error> {
    // 1. Check subscription
    let sub_manager = L1_STATE_SUBSCRIPTION_MANAGER;
    if !sub_manager.is_subscribed(account, slot) {
        return Err(Error::NotSubscribed);
    }

    // 2. Read from L1 state cache
    let value = l1_state_cache.get(account, slot)?;

    // 3. Log access
    current_block_access_log.push(L1StateAccessEntry {
        account,
        slot,
        value,
    });

    // 4. Return value
    Ok(value)
}
```

### Proof system changes

Batch proofs must verify:
1. All L1 state accesses are for subscribed slots
2. `L1StateAccessLog` matches actual reads during execution
3. Values in log match L1 state at `finalizeTempo()` Tempo block
4. Log hash is committed in batch submission

### TIP-403 dynamic subscription

Sequencer watches for TIP-20 transfers involving new addresses:

```solidity
function onTIP20Transfer(address from, address to) external {
    // Read transferPolicyId from L1
    uint256 policyId = readTempoStorageSlot(l1ZoneToken, TRANSFER_POLICY_SLOT);

    // Calculate policy set slots
    bytes32 innerSlot = keccak256(abi.encode(policyId, uint256(2)));
    bytes32 fromSlot = keccak256(abi.encode(from, innerSlot));
    bytes32 toSlot = keccak256(abi.encode(to, innerSlot));

    // Subscribe if not already subscribed
    if (!subscriptionManager.isSubscribed(tip403Registry, fromSlot)) {
        subscriptionManager.autoSubscribePolicySet(policyId, from);
    }
    if (!subscriptionManager.isSubscribed(tip403Registry, toSlot)) {
        subscriptionManager.autoSubscribePolicySet(policyId, to);
    }
}
```

## Migration Path

Zones can start with subscriptions and later add state declarations if needed:

1. **Phase 1**: Subscription-only model (simpler UX, suitable for TIP-20 focused zones)
2. **Phase 2**: Add type 0x7A declarations (for advanced use cases, one-time reads)
3. **Hybrid**: Allow both - users choose based on usage pattern

## Future Enhancements

### Subscription pools

Allow multiple users to share subscription costs:
- User A subscribes for 12 months
- User B extends the same slot for 6 months
- Expiry becomes max(A's expiry, B's expiry)

### Dynamic fee adjustment

Sequencer adjusts `monthlySubscriptionFee` based on:
- Total number of subscribed slots
- L1 gas prices (affects monitoring cost)
- Zone token price (denominated in stablecoin equivalent)

### Subscription NFTs

Issue transferable NFTs representing active subscriptions:
- Trade subscription rights on secondary markets
- Use as collateral in DeFi
- Simplify subscription management for DAOs
