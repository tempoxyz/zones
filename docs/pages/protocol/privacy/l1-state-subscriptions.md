# L1 State Subscriptions

This document describes the L1 state subscription model for Tempo Zones, an alternative to transaction-level state declarations.

## Overview

Instead of requiring users to declare L1 state in each transaction, the subscription model allows:

1. **Upfront subscription** to specific L1 state slots, i.e. (account, slot) pairs
2. **Automatic subscription** for TIP-403 policy state (zone token transfers)
3. **Monthly subscription fees** paid to the sequencer
4. **Real-time state sync** by the sequencer
5. **Complete replay capability** via L1 state access logs

## Architecture

### Components

#### 0. ZoneConfig (Predeploy at 0x1c00000000000000000000000000000000000002)

**Central zone metadata and L1 state references:**

- **Single source of truth**: All zone metadata stored in one place
- **L1 sequencer reads**: Sequencer address read from L1 ZonePortal (not replicated on L2)
- **Auto-subscribed slots**: Portal sequencer slots permanently subscribed at genesis
- **Referenced by all contracts**: Other L2 contracts use ZoneConfig instead of duplicating metadata

**Key functions:**
```solidity
function sequencer() external view returns (address);        // Read from L1
function pendingSequencer() external view returns (address); // Read from L1
function isSequencer(address) external view returns (bool);  // Convenience check
function zoneToken() external view returns (address);        // Zone token address
function l1Portal() external view returns (address);         // L1 ZonePortal address
```

**Benefits:**
- Eliminates 100+ lines of duplicate sequencer transfer logic per contract
- L1 ZonePortal is single source of truth for sequencer
- Sequencer changes on L1 automatically visible on L2 (after Tempo block finalization)
- Smaller, simpler contracts

#### 1. L1StateSubscriptionManager (Predeploy at 0x1c00000000000000000000000000000000000001)

Manages subscriptions to L1 state slots:

- **Subscribe**: Users call `subscribe(account, slot, days)` and pay `dailySubscriptionFee * days` in zone tokens
- **Auto-subscription**: TIP-403 policy state for the zone token is automatically subscribed at genesis (permanent)
- **Fee management**: Sequencer sets `dailySubscriptionFee` to cover L1 sync costs
- **Expiry tracking**: Subscriptions expire after the paid period unless extended

**Key storage:**
```solidity
mapping(bytes32 => uint64) public subscriptionExpiry;  // keccak256(abi.encode(account, slot)) => expiryTimestamp
uint128 public dailySubscriptionFee;                   // Set by sequencer
```

**Automatic TIP-403 subscriptions:**
- Zone token's `transferPolicyId` (permanent)
- TIP-403 `_policyData[transferPolicyId]` (permanent, added by sequencer)
- Dynamic: `policySet[transferPolicyId][address]` entries (the sequencer has to provide all accesses of this type, meaning they have to maintain a full list of all nonzero values in the policy set)

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
2. User calls `subscriptionManager.subscribe(account, slot, days)`
3. Contract transfers `dailySubscriptionFee * days` zone tokens to sequencer
4. Subscription becomes active immediately
5. User can now call contracts that read `(account, slot)` via TempoState

### TIP-20 transfers (automatic subscription)

1. At genesis: Zone token's `transferPolicyId` slot is auto-subscribed (permanent)
2. After genesis: Sequencer reads `transferPolicyId` from L1 and calls `autoSubscribePolicyState(transferPolicyId)`
3. Policy data subscription becomes permanent

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
| **Economics** | Gas per declaration | Daily subscription fee |

## Security Considerations

### Subscription expiry

- Contracts must handle `SubscriptionExpired` errors gracefully
- Users should monitor expiry and extend before it lapses
- Auto-subscribed TIP-403 slots are permanent (never expire)

### Sequencer trust assumptions

- The correct handling of L1 state subscription is part of the proof system
- An address subscribed through the contract or part of the permanent subscription set MUST be provided by the sequencer 

### Replay attacks

- L1StateAccessLog records value **at time of access**, not current value
- Replaying with stale values will fail state transition verification
- Proofs ensure logs match actual L1 state at the finalized Tempo block

## Economics

### Subscription pricing

Sequencer sets `dailySubscriptionFee` to cover:
- L1 monitoring costs (events, state polling)
- Storage costs for cache
- Proof generation overhead for access logs

Suggested model:
```
baseFee = cost to monitor one slot for 1 day
fee per slot = baseFee * riskMultiplier
```

Where `riskMultiplier` accounts for:
- Volatility of the slot (frequent updates = higher risk)
- Number of subscribers (amortization)

### Fee distribution

All subscription fees are transferred to the sequencer.

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
3. Values in log match L1 state defined by `tempoStateRoot`
4. Log hash is committed in batch submission

### TIP-403 subscriptions

All accesses to `TIP403Registry.policySet[policyId][address]` by `TIP403Registry` are considered to be subscribed addresses. Note that this cannot be cleanly mapped to `(account, slot)` pairs because the slot is hashed. Instead, the sequencer has to look at the actual call path in the `TIP403Registry` contract to determine that it is a subscribed address.

This also means that: If another contract loads any `TIP403Registry` address that is not explicitly subscribed, it is considered NOT subscribed. This is because the sequencer cannot reliably determine what the origin of the call is (without doing code introspection hacks that would lead to nondeterminism)