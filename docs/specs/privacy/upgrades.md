# Zone upgrade process

This document describes both the protocol rules and the operational process for zone hard forks.

The core idea is same-block activation: a zone switches to the new rules in the zone block that imports the Tempo block where the fork activates. Nothing on the zone waits for "the next block" once the triggering Tempo block has been imported.

For the high-level system model, see the [overview](./overview). For the proof interface affected by upgrades, see the [Zone Prover Design](./prover-design).

## Upgrade model

A zone hard fork is defined by:

- **Fork Tempo block number (`F`)**: the Tempo block number at which the fork activates.
- **`forkVerifier`**: the new `IVerifier` contract deployed on Tempo as part of the hard fork.

`F` is embedded in the zone node's chain specification and prover program. The portal does not need to store it explicitly.

A zone block is a **post-fork zone block** if the Tempo block it imports via `advanceTempo` has number `>= F`.

A **fork-spanning batch** is a batch containing both pre-fork and post-fork zone blocks.

### Activation rule

The fork activates in the zone block that imports the fork Tempo block, not the following block. The entire zone block uses the new execution rules:

- `advanceTempo`
- all user transactions
- `finalizeWithdrawalBatch`

The EVM is configured with the new ruleset before any transaction in that zone block executes.

The trigger is the imported Tempo block number, not wall-clock time. This keeps activation unambiguous even if a zone is behind and is catching up later.

Same-block activation is required for correctness. If a fork changes the Tempo header format or the behavior of zone predeploys, the new parsing and validation rules must already be active when that first post-fork Tempo header is imported.

### Verifier routing

The portal maintains two verifier slots. The batch submitter chooses which verifier to target, and the portal enforces that only recognized verifier addresses can be used:

```solidity
address public verifier;             // older verifier slot
address public forkVerifier;         // newer verifier slot
uint64  public forkActivationBlock;  // Tempo block where forkVerifier was installed

function submitBatch(address targetVerifier, uint64 tempoBlockNumber, ...) external {
    require(
        targetVerifier == verifier || targetVerifier == forkVerifier,
        "unknown verifier"
    );

    if (targetVerifier == verifier && forkActivationBlock != 0) {
        require(tempoBlockNumber < forkActivationBlock, "use fork verifier");
    }

    require(IVerifier(targetVerifier).verify(...), "invalid proof");
}
```

This ensures that a post-fork batch cannot be submitted against an old verifier after the fork has activated.

`forkActivationBlock` is set to `block.number` when `ZoneFactory.setForkVerifier()` runs during the Tempo hard fork. No separate on-chain copy of `F` is needed.

The verifier interface itself should stay stable across forks whenever possible. New proof parameters should be carried inside the opaque `verifierConfig` payload so that portal interfaces do not need to change for every upgrade.

### Two-verifier invariant

At most two verifiers are active at any time.

When a new fork happens:

```text
verifier            = forkVerifier
forkVerifier        = new_fork_verifier
forkActivationBlock = block.number
```

For the first fork, `verifier` stays at the original verifier and `forkVerifier` is populated for the first time.

This rolling two-slot design lets zones that are slightly behind keep submitting older batches while still enforcing the new verifier for post-fork work. It also means that a zone cannot lag forever: once fork `N` activates, the verifier from fork `N-2` is gone.

### Prover and node behavior

The off-chain components must switch in lockstep with the verifier rotation:

- **Pre-fork batches**: all imported Tempo blocks are `< F`, so the node uses the old prover and submits to the older verifier slot.
- **Post-fork batches**: at least one imported Tempo block is `>= F`, so the node uses the new prover and submits to `forkVerifier`.
- **Fork-spanning batches**: a single proof may cover both eras, but it must use the new prover and target `forkVerifier`.

The new prover program contains both rule sets. For each zone block in the batch it checks the imported Tempo block number and applies the correct branch:

- Tempo block `< F`: old rules
- Tempo block `>= F`: new rules

Each successive prover is therefore a superset of the previous one. That is what allows the newest verifier to continue accepting older-era blocks when needed.

The zone node follows the same rule split:

- It determines the ruleset by inspecting the next imported Tempo block.
- If the fork changes predeploy behavior, it injects the new bytecode at the predeploy addresses before `advanceTempo` executes in the first post-fork zone block.
- It halts if Tempo signals a protocol version above the highest version the binary supports.

#### Fork signaling

`ZoneFactory` maintains a `protocolVersion` counter that increments at each hard fork.

Each zone node binary embeds the highest protocol version it supports. Before building the next zone block, the node checks whether the Tempo block it is about to import bumped `protocolVersion` beyond that limit. If so, the node refuses to build the block and halts with a clear upgrade error instead of risking divergence.

## Artifacts

Each zone hard fork requires the following artifacts. All are prepared before the fork and deployed or released atomically at fork time.

| Artifact | Embeds `F` | Description |
|----------|:----------:|-------------|
| Zone node binary | Yes | Contains both old and new execution rules. Embeds `F` in the chain spec and `MAX_SUPPORTED_PROTOCOL_VERSION` for fork signaling. |
| Prover program | Yes | Dual-rule binary. Applies old rules for blocks importing Tempo `< F`, new rules for `>= F`. Produces a new verification key covering the complete program. |
| Verifier contract | No | `IVerifier` deployed on Tempo with the verification key from the new prover program. |
| Predeploy bytecode | N/A | Updated bytecode for `TempoState`, `ZoneInbox`, `ZoneOutbox`, and `ZoneConfig` if the fork changes their behavior. |
| Tempo system transaction payload | No | Encoded call bundle that deploys the verifier, rotates verifier slots through `ZoneFactory.setForkVerifier()`, and increments `protocolVersion`. |

## Timeline

A zone hard fork proceeds in three phases:

```text
Pre-fork                          Fork block F                    Post-fork
---------------------------------+-------------------------------+--------------------------
Build artifacts                  | Deploy verifier contract      | Zones activate new rules
Release node binary + prover     | Rotate verifiers on portals   |   when they import block F
Operators upgrade nodes          | Increment protocolVersion     | New prover handles new era
```

### Pre-fork

1. Determine `F`, the Tempo block number where the fork activates.
2. Build the new prover program and extract its verification key.
3. Build the verifier contract parameterized by that verification key.
4. Build the zone node binary with the new rules and supported `protocolVersion`.
5. Prepare any predeploy bytecode updates the fork requires.
6. Prepare the Tempo system transaction payload that will deploy the verifier, rotate verifier slots, and increment `protocolVersion`.
7. Release the new node binary and prover program ahead of time so operators can upgrade before `F`.
8. Announce the upgrade window and required binary versions.

### Fork block execution

The Tempo hard fork block performs the following as system transactions:

1. Deploy the new `IVerifier`.
2. Call `ZoneFactory.setForkVerifier(forkVerifier)`.
3. For each registered portal:
   - promote the existing `forkVerifier` into `verifier`
   - install the new verifier into `forkVerifier`
   - set `forkActivationBlock = block.number`
4. Increment `ZoneFactory.protocolVersion`.

New zones created after this block use the newest verifier as their initial verifier.

### Post-fork

No manual on-chain action is required from the zone operator once the fork block lands.

Each zone transitions automatically when it eventually imports block `F`:

1. The zone node reaches Tempo block `F`.
2. It switches execution to the new rules for that zone block.
3. If needed, it injects new predeploy bytecode before `advanceTempo`.
4. The batch builder starts targeting the new prover and verifier for all post-fork or fork-spanning batches.
5. Settlement continues using the rotated verifier slots already installed on Tempo.

## Failure modes

### Operator did not upgrade

An outdated node does not know about the new rules. When it encounters the Tempo block that bumped `protocolVersion`, it detects that the network is ahead of its `MAX_SUPPORTED_PROTOCOL_VERSION` and halts cleanly with an upgrade error.

No invalid blocks are produced. Settlement pauses because no new batches are submitted. User funds remain safe in the portal until the operator upgrades.

### Node upgraded but prover is stale

The node may be able to build correct post-fork blocks while still lacking the new prover binary. In that case, zone execution can continue locally, but settlement of post-fork batches pauses because the old prover cannot generate proofs accepted by `forkVerifier`.

Once the new prover is installed, it can prove the backlog of post-fork batches.

### Zone is behind Tempo

If a zone is catching up and has not yet imported block `F`, the fork does not activate for that zone yet. It continues executing old rules for all imported Tempo blocks `< F`.

This is expected, but it interacts with the two-verifier invariant. A zone that falls more than one full fork cycle behind can lose the ability to submit its oldest historical batches once their verifier ages out of the two-slot window.

## Verifier lifecycle

The portal always keeps two verifier slots plus the cutoff block that separates them:

| Event | `verifier` | `forkVerifier` | `forkActivationBlock` |
|-------|------------|----------------|----------------------|
| Zone creation | V0 | `address(0)` | 0 |
| Fork 1 | V0 | V1 | F1 |
| Fork 2 | V1 | V2 | F2 |
| Fork 3 | V2 | V3 | F3 |

This implies:

- Between fork 1 and fork 2, V0 may still verify batches with `tempoBlockNumber < F1`, while V1 verifies everything newer.
- After fork 2, V0 is gone. V1 becomes the "old" slot and is restricted to `tempoBlockNumber < F2`.
- The pattern repeats indefinitely.

The older slot exists to support lagging zones, not to create a second active upgrade path. Post-fork work must always use the newest verifier slot.
