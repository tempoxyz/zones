# Zone upgrade process

This document describes the end-to-end process for executing a zone hard fork. For the protocol rules governing activation, verifier routing, and fork signaling, see [Hard fork activation](./overview.md#hard-fork-activation).

## Artifacts

Each zone hard fork requires the following artifacts. All are prepared before the fork and deployed or released atomically at fork time.

| Artifact | Embeds `F` | Description |
|----------|:----------:|-------------|
| Zone node binary | Yes | Contains both old and new execution rules. Embeds `F` in the chain spec and `MAX_SUPPORTED_PROTOCOL_VERSION` for fork signaling. |
| Prover program | Yes | Dual-rule binary. Applies old rules for blocks importing L1 `< F`, new rules for `>= F`. Produces a new verification key covering the complete program. |
| Verifier contract | No | `IVerifier` deployed on Tempo L1 with the verification key from the new prover program. |
| Predeploy bytecode | N/A | Updated bytecode for zone predeploys (TempoState, ZoneInbox, ZoneOutbox, ZoneConfig) if the fork changes their behavior. Not every fork requires this. |
| L1 system transaction payload | No | Encoded call to `ZoneFactory.setForkVerifier(forkVerifier)` and `protocolVersion` increment, executed as part of the L1 hard fork block. |

## Timeline

A zone hard fork proceeds in three phases:

```
Pre-fork                          Fork block F                    Post-fork
─────────────────────────────────┬──────────────────────────────┬──────────────────────────
Build artifacts                  │ Deploy verifier contract     │ Zones activate new rules
Release node binary + prover     │ setForkVerifier() rotates    │   on import of block F
Operators upgrade nodes          │   verifiers on all portals   │ Provers use new rules for
                                 │ Increment protocolVersion    │   post-fork blocks
                                 │                              │ Settlement resumes with
                                 │                              │   fork verifier
```

### Pre-fork

1. **Determine `F`.** Choose the L1 block number at which the fork activates. If the L1 fork uses a timestamp, `F` is the first L1 block at or after that timestamp — this derivation is an off-chain concern since the portal does not store `F`.

2. **Build the prover program.** The new prover is a superset of the previous one: it uses the previous prover's complete logic as its "old rules" branch and adds the new rules for blocks at `>= F`. Build the program and extract the verification key.

3. **Build the verifier contract.** Compile the `IVerifier` contract parameterized with the new verification key. Prepare the deployment bytecode for inclusion in the L1 fork block.

4. **Build the zone node binary.** The binary embeds `F` in its chain spec and sets `MAX_SUPPORTED_PROTOCOL_VERSION` to the new protocol version. It contains both old and new execution rules, switching based on the L1 block number imported by `advanceTempo`.

5. **Prepare predeploy bytecode diffs** (if applicable). If the fork changes zone predeploy contracts, prepare the new bytecode. The node binary must include the injection logic: new bytecode is written to the predeploy addresses at the start of the fork zone block's state transition, before `advanceTempo` executes.

6. **Prepare the L1 system transaction payload.** Encode the calls that the L1 fork block will execute:
   - Deploy the verifier contract.
   - `ZoneFactory.setForkVerifier(forkVerifier)` — rotates the centralized verifier state and increments `protocolVersion`. This is a single O(1) call that applies to all zones.

7. **Release binaries.** Publish the zone node binary and prover program. Operators can upgrade at their convenience — no on-chain transaction is required. The node runs under old rules until the fork L1 block arrives.

8. **Announce the upgrade window.** Communicate `F`, the expected fork date, and the required binary versions to zone operators.

### Fork block execution

The Tempo L1 hard fork block executes the following as system transactions:

1. Deploy the fork `IVerifier` contract with the new verification key.
2. Call `ZoneFactory.setForkVerifier(newForkVerifier)`. This is a single O(1) call that updates centralized state in the factory:
   - If `forkVerifier != address(0)`: `verifier = forkVerifier` (promote previous fork verifier)
   - `forkVerifier = newForkVerifier` (install new fork verifier)
   - `forkActivationBlock = block.number` (record cutoff — old verifier rejected for batches at or past this L1 block)
   - `protocolVersion++`
   - For the first fork, `verifier` retains its original value and `forkVerifier` is populated for the first time.

No per-portal iteration is needed — verifier state is centralized in `ZoneFactory` and portals delegate validation to the factory via `validateVerifier()` at batch submission time.

After this block, the factory's `verifier()` returns the current active verifier for new zones.

### Post-fork

No manual action is needed. Each zone transitions automatically:

1. The zone node imports L1 blocks sequentially. When it reaches block `F`, it detects the fork and configures the EVM with new rules for that zone block (same-block activation).
2. If the fork includes predeploy changes, the node injects new bytecode at the predeploy addresses before `advanceTempo` runs.
3. The zone block header's `protocol_version` field is set to the new protocol version.
4. The node decides which prover to invoke per batch. Pre-fork batches (all L1 blocks `< F`) use the old prover and target the old verifier. Post-fork or fork-spanning batches (any L1 block `>= F`) use the new prover and target the fork verifier. The new prover applies old rules for pre-fork blocks and new rules for post-fork blocks within the same proof.
5. The fork verifier is available on-chain from block `F`, so post-fork proofs can be submitted immediately.

## Failure modes

### Operator did not upgrade

The outdated zone node binary does not know about `F`. When it encounters the L1 block that bumped `protocolVersion`, it detects that the new version exceeds its `MAX_SUPPORTED_PROTOCOL_VERSION` and halts with an error directing the operator to upgrade.

No invalid blocks are produced. Settlement pauses because no new batches are submitted. User funds in the portal remain safe. The zone resumes normal operation once the operator installs the new binary.

### Node upgraded but prover is stale

The zone node produces correct post-fork blocks, but with only the old prover available it cannot generate proofs acceptable to the fork verifier. The node continues producing blocks, but settlement of post-fork batches pauses. Pre-fork batches already proven can still be submitted to the old verifier. Once the new prover is installed, it can prove the backlog of post-fork batches.

### Zone is behind L1

If a zone is catching up and has not yet reached L1 block `F`, the fork does not activate until the zone imports block `F`. The zone continues under old rules for all blocks importing L1 blocks `< F`. This is by design — the trigger is the L1 block number the zone imports, not the current L1 head.

A zone that falls more than one full fork cycle behind risks having its oldest batches become unsubmittable. The two-verifier invariant means the N-2 verifier is deprecated when fork N activates. If the zone still has unproven batches from before fork N-1, those batches can no longer be verified.

## Verifier lifecycle

`ZoneFactory` maintains two verifier slots and a `forkActivationBlock` cutoff. The batch submitter specifies which verifier to use; the portal delegates validation to the factory, which checks that it is one of the two recognized addresses and that the old verifier is only used for batches predating the fork. Across successive forks, the slots rotate:

| Event | `verifier` | `forkVerifier` | `forkActivationBlock` |
|-------|-----------|----------------|----------------------|
| Factory deployment | V0 | `address(0)` | 0 |
| Fork 1 (block F1) | V0 | V1 | F1 |
| Fork 2 (block F2) | V1 | V2 | F2 |
| Fork 3 (block F3) | V2 | V3 | F3 |

At each fork, the previous `forkVerifier` is promoted to `verifier`, the new fork verifier takes the `forkVerifier` slot, and `forkActivationBlock` is updated to the current L1 block number. The verifier that was in the `verifier` slot is deprecated.

The `forkActivationBlock` enforces compliance: submissions targeting `verifier` must have `tempoBlockNumber < forkActivationBlock`. This prevents a non-upgraded or malicious node from submitting post-fork batches proved under old rules to the old verifier.

This means:
- Between fork 1 and fork 2: V0 accepts batches with `tempoBlockNumber < F1`, V1 accepts all batches. Zones still catching up can submit pre-fork-1 batches using V0.
- After fork 2: V0 is deprecated. V1 accepts batches with `tempoBlockNumber < F2`, V2 accepts all batches.
- The pattern continues: at any point, exactly the two most recent verifiers are active, with the older one restricted to pre-fork batches.

Each prover program is a superset of the previous one (fork N's prover includes fork N-1's logic as its old-rules branch), so the fork N verifier can verify batches containing blocks from any prior era.
