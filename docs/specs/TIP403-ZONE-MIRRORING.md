# TIP-403 Zone Mirroring

Zone-side TIP-20 transfers enforce L1 TIP-403 policies via L1 state mirroring.

## Design

1. Zone nodes run L1 full nodes and maintain L1 state
2. `TIP403Mirror` predeploy reads L1 TIP-403 state (node provides values)
3. Prover validates all L1 state accesses against `l1StateRoot`

```
L1 TIP403Registry (0x403c...0000)
        │
        │  Zone node has L1 state
        ▼
Zone TIP403Mirror (0x403c...0001)
        │
        │  isAuthorized() reads from node's L1 state
        ▼
Zone TIP-20 transfer (same gas cost as L1)
        │
        ▼
Prover generates Merkle proofs for accessed L1 state
```

## Gas Cost

Same as L1: ~2,600 gas for `isAuthorized()` (2 SLOADs).

## Proof Overhead

Prover includes Merkle proofs for unique (policyId, account) pairs per batch:
- 1 account proof (~500 bytes)
- 1 proof per unique storage slot (~200 bytes each)
- Typical batch: ~10-15 KB added to proof

## TIP403Mirror Predeploy

Address: `0x403c000000000000000000000000000000000001`

Mirrors L1 `TIP403Registry` storage layout. Zone node intercepts storage reads and returns L1 values.

```solidity
interface ITIP403Mirror {
    function isAuthorized(uint64 policyId, address user) external view returns (bool);
}
```

## IVerifier Update

Add `l1StateRoot` parameter:

```solidity
interface IVerifier {
    function verify(
        bytes32 processedDepositsHash,
        bytes32 pendingDepositsHash,
        bytes32 newProcessedDepositsHash,
        bytes32 prevStateRoot,
        bytes32 newStateRoot,
        bytes32 l1StateRoot,           // NEW
        bytes32 expectedQueue2,
        bytes32 updatedQueue2,
        bytes32 newWithdrawalsOnly,
        bytes calldata verifierData,
        bytes calldata proof
    ) external view returns (bool);
}
```

## L1 State Staleness

Zone uses L1 state from latest processed deposit's block. Recommended lag: <10 L1 blocks (~2 minutes).
