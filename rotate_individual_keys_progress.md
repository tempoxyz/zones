# Individual identity rotation live test progress

## Objective

Exercise the `xtask_individual_keys` implementation and the portal runbook against a fresh local Tempo L1 with three sequencers and one RPC-only Zone node. Follow the guide step by step, record exact evidence, and fix any workflow defect demonstrated by the run.

## Test environment

- Date: 2026-08-13 (America/New_York)
- Zones repository: `/Users/adityapk/github/tempoxyz/xtask_admin`
- Zones branch: `xtask_individual_keys`
- Harness: `/Users/adityapk/github/tempoxyz/aditya_tests`
- Portal guide: `/Users/adityapk/github/tempoxyz/dev-platform/portal/src/pages/runbooks/zones/rotate-p2p-and-secp256k1-key.mdx`
- Planned topology: three quorum sequencers plus one RPC-only follower

## Progress

### Harness inspection — PASS

- Read the `run-zone-dev` and `zones-deploy-debug` skill instructions.
- Confirmed the harness repository is present. Its pre-existing `README.md` modification will be preserved.
- Confirmed no harness run is active.
- Selected run tag `ikr` (short enough for macOS IPC paths), default ports, native Tempo L1, three sequencers, one RPC-only follower, and quorum 2.
- The harness starts nodes from a shared `zone/manifest.toml` and per-node `*-p2p.key` / `*-secp256k1.key` files. This permits a production-shaped rollout by changing those files only while the target node is stopped.
- Harness controls will be used only for exact node restarts and final cleanup; Portal and leadership changes will use the new admin xtasks from the guide.

### Fresh topology startup — PASS

Command:

```bash
bash ./harness up \
  --zone-repo /Users/adityapk/github/tempoxyz/xtask_admin \
  --tempo-bin /Users/adityapk/github/tempoxyz/tempo/target/release/tempo \
  --sequencers 3 --rpc-nodes 1 --quorum 2 \
  --run-tag ikr
```

Result: **PASS**.

- Run directory: `/Users/adityapk/github/tempoxyz/aditya_tests/.harness/runs/ikr`
- Harness rebuilt ZonePortal artifacts, `tempo-zone` release, and `tempo-xtask` debug before launch.
- Native Tempo L1 and all four Zone nodes are owned by supervisor PID `4523`.
- Startup canonical check converged at Zone height 65.
- Initial leader: `sequencer-1` / `0xE6b3367318C5e11a6eED3Cd0D850eC06A02E9b90`, epoch 1.

### Guide configuration and initial preflight — PASS

- Created public `ops/zone-admin.toml` pointing at the generated current manifest, L1 RPC, Portal, and all four operator RPCs.
- Stored the known local Portal admin test key in an owner-only `ops/portal-admin.key` file. Private material is not reproduced here.
- Ran `tempo-xtask admin check --config ops/zone-admin.toml --json`.
- Result: `ok=true`, desired topology verified, Portal version 1, threshold 2, all 4/4 nodes reachable and canonical, all 3 sequencers promotion-ready, decryption-key availability passed, L1 batch 12, and every node advanced from height 129 to 139 during observation.

### Rotation 1: sequencer-2 (follower) — PASS

- Current member: `0x88C0e901bd1fd1a77BdA342f0d2210fDC71Cef6B`
- Current version: 1; next version: 2.
- `admin identity prepare --json`: **PASS**. Wrote owner-only independent key files; reported P2P public key `0xd2ef…3d37` and sequencer address `0x6d1978…7ad6` without exposing private material.
- Funded the replacement address with local pathUSD; receipt `0xeffe4c…6aeb` succeeded and balance became `1000000000000000`.
- Prepared `zone-v2.toml` changing only sequencer-2's public identities and version 1 -> 2.
- `admin sequencer-set replace` dry run: **PASS**. It verified Portal/admin, old/new member, exact next-manifest membership, retained leader, threshold 2, and version 1 -> 2, then simulated the call.
- `admin sequencer-set replace --execute`: **PASS**. Transaction `0xa4bc3c…65c9` finalized with exact version 2 membership.
- Installed the v2 manifest and replacement sequencer-2 key files, then restarted sequencer-2.
- `admin check --wait-ready --node sequencer-2 --zone-manifest zone-v2.toml`: **PASS**. The node reports the new address/P2P key, version 2, promotion readiness, no pending transitions, and its persisted canonical block.

#### Live finding: whole-cluster restart gate is unsuitable mid-rollout

The harness `node restart` convenience control waits for whole-cluster tip convergence. After a membership/P2P identity change, the restarted follower intentionally runs the next manifest while the old leader still runs the previous manifest, so whole-cluster convergence cannot occur yet. The harness control remains waiting even though the node is healthy under the guide's node-scoped gate. For the remaining rollout, use exact `node stop` + `node start` controls and the guide's `admin check --wait-ready --node` gate.

#### Live finding: the handoff relay must run the next manifest

- Restarted sequencer-3 and the RPC-only node on v2 before leadership handoff; both became healthy and canonical.
- The guide's initial `--via <current-leader-node>` dry run passed, but execution failed safely with `target is not a manifest member`. The old leader was still running v1 and therefore could not resolve sequencer-2's replacement identity.
- Retried through sequencer-3, a healthy follower already running v2. Transaction `0xf954e8…a1a6` finalized sequencer-2 as leader at epoch 2.
- Required guide correction: in rolling-membership mode, `--via` must identify a retained, healthy sequencer already running the next manifest. The xtask preflight should reject an old-manifest relay before submission.

#### Live finding: handoff readiness currently delays former-leader recovery

- Immediately after handoff, the former leader (sequencer-1) stopped serving because its v1 manifest could not resolve the replacement leader. The new leader, sequencer-3, and RPC-only follower continued producing a canonical chain.
- `admin leader set` kept polling because it requires finalized Portal Zone height to advance before returning. Portal settlement resumed only after sequencer-1 was started on v2; the command then completed successfully.
- Required xtask correction: for `--rolling-membership`, return after finalized Portal leader/epoch and target-node agreement. The guide can then immediately restart the former leader on the new manifest and perform the normal full-cluster progress gate.

### Rotation 1 recovery and convergence — PASS

- Started sequencer-1 with the installed v2 manifest.
- All four nodes converged again; Portal settlement advanced from the stalled height 250 to at least 870.
- The handoff command returned `ok=true`, leader sequencer-2, epoch 2.

### Rotation 2: sequencer-3 (follower) — PASS

- Prepared independent replacement keys: P2P public key `0x126e28…6017`, sequencer address `0x2238e3…cf0a`; private material remained owner-only.
- Funded the replacement address; L1 funding transaction `0xc68d0b…3e87` was accepted.
- Prepared v3 changing only sequencer-3's two public identities and version 2 -> 3.
- Replacement dry run passed; execute transaction `0x3be0fe…417e` finalized exact v3 membership while retaining sequencer-2 as leader.
- Restarted sequencer-3 with its new keys and v3; its node-scoped readiness/canonical gate passed. Restarted RPC-1 and sequencer-1 on v3, leaving leader sequencer-2 on v2.
- Dry-ran and executed leadership handoff to sequencer-3 through sequencer-1, a retained follower already running v3. Portal finalized sequencer-3 at epoch 3.
- The former leader exited shortly after handoff. An initial `node start` raced that exit and observed the still-running old process; a second exact start was needed after it had exited. This reinforces that rollout automation must confirm a fresh PID/loaded manifest, not only momentary RPC reachability.
- The first full-cluster check correctly failed while the epoch-3 activation remained pending. After activation, all four nodes reported zero pending transitions, canonical convergence, progress, and resumed settlement.
- Final v3 check: `ok=true`, exact version 3, leader sequencer-3, epoch 3, all invariants passed.

### Rotation 3: sequencer-1 (follower) — PASS

- Prepared independent replacement keys: P2P public key `0x69ff38…1769`, sequencer address `0x8cb324…557e`; private material remained owner-only.
- Funded the replacement address; L1 funding transaction `0xc8fec8…8425` was accepted.
- The first v4 manifest validation failed safely because `leader_ed25519_public_key` still referenced sequencer-1's old P2P key, which the replacement removed. Corrected it to the finalized leader sequencer-3's P2P key.
- Required guide correction: if `leader_ed25519_public_key` points to the identity being replaced, update it to the finalized leader's P2P key in the next manifest.
- Replacement dry run passed; execute transaction `0x21ed72…afbb` finalized exact v4 membership while retaining sequencer-3 as leader.
- Restarted sequencer-1 with its new keys and v4; node-scoped readiness and identity checks passed. Restarted RPC-1 and sequencer-2 on v4.
- Dry-ran and executed leadership handoff through next-manifest follower sequencer-2. Transaction `0x76c8ee…6f05` finalized sequencer-1 as leader at epoch 4.
- Waited until former leader sequencer-3 had actually exited, then started it on v4. This avoided the restart race observed in rotation 2.
- Final admin check: `ok=true`, exact version 4, leader sequencer-1, epoch 4, all three replacement addresses/P2P identities matched, all four nodes were canonical and progressing, and L1 settlement batch 146 covered Zone height 1460.

### Encrypted deposit and harness verification — PASS

- Submitted one amount-1000 encrypted deposit using the normal `tempo-xtask deposit` path through the harness workload client; transaction `0x396e56…8713`, zero deposit failures, and a clean workload stop.
- Harness verification returned `ok=true`: the deposit account changed from L1 `1000000000000` / Zone `0` to L1 `999999999000` / Zone `1000`, preserving total balance exactly.
- All four nodes converged at the verification target, Portal settlement reached at least Zone height 1610, the withdrawal queue was empty, and there were no transaction failures.

## Implementation changes proven necessary by the live test

- `admin leader set --rolling-membership` now preflights both the target and `--via` relay against the supplied next manifest and finalized Portal membership.
- Rolling handoff completion now requires finalized Portal leader/epoch plus target agreement, but deliberately leaves Zone progress/settlement to the post-restart full-cluster gate. Normal non-rolling handoffs still require progress before returning.
- The runbook now uses the rotated next-manifest target to relay its own handoff (a retained next-manifest follower also works), explains immediate former-leader recovery and restart verification, and handles `leader_ed25519_public_key` when its referenced identity is rotated.

## Validation

- `cargo fmt --all --check`: PASS (stable-toolchain warnings only for nightly rustfmt options).
- `cargo test -p tempo-xtask`: PASS, 34 tests.
- `cargo clippy -p tempo-xtask --all-targets -- -D warnings`: PASS.
- Fixed binary dry run with the rotated target as both `--target` and `--via`: PASS, confirming the simplified runbook shape is accepted.
- Portal `corepack pnpm --dir portal build`: PASS. It reported four unrelated pre-existing dead links elsewhere in the documentation.

## Cleanup — PASS

- Stopped the exact harness run with `harness --run-dir .harness/runs/ikr down`.
- The supervisor confirmed `stopped=true`; the run control socket no longer exists, as expected after shutdown.
- Preserved the harness repository's pre-existing `README.md` modification and all unrelated workspace changes.
