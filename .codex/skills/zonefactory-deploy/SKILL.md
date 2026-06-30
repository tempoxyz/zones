---
name: zonefactory-deploy
description: Deploy a fresh ZoneFactory contract to Moderato Tempo L1 after ABI or implementation-breaking changes, verify the deployment, and update repo constants and docs.
---

# ZoneFactory Deploy

Use this skill when a shared Moderato `ZoneFactory` must be redeployed because the factory, portal, messenger, verifier, or related ABI changed.

## Workflow

1. Read `xtask/src/zone_utils.rs` and `docs/ZONES.md` to confirm the current `MODERATO_ZONE_FACTORY` address and docs table entry.
2. Build the Foundry reference contracts from `specs/ref-impls`.
3. Deploy `src/l1/ZoneFactory.sol:ZoneFactory` to Moderato.
4. Verify the new factory has code, `zoneCount()` returns `0`, and `verifier()` returns a non-zero address.
5. Update `MODERATO_ZONE_FACTORY`, its explorer comment, `docs/ZONES.md`, and any other `rg` hits for the old address.
6. Run Rust formatting/checks that cover the changed constants.

## Commands

```bash
cd specs/ref-impls
export ETH_RPC_URL=https://rpc.moderato.tempo.xyz
export PRIVATE_KEY=<deployer_private_key>

forge build
forge create --broadcast --rpc-url "$ETH_RPC_URL" --private-key "$PRIVATE_KEY" src/l1/ZoneFactory.sol:ZoneFactory
```

For manual deployments, prefer replacing `--private-key "$PRIVATE_KEY"` with `--interactive` so the key is not written into shell history or process arguments. If the user explicitly requires a non-interactive command and has already provided `PRIVATE_KEY` through the environment, use `--private-key "$PRIVATE_KEY"` without echoing the value.

After deployment:

```bash
export ZONE_FACTORY=0x...

cast code "$ZONE_FACTORY" --rpc-url "$ETH_RPC_URL"
cast call "$ZONE_FACTORY" "zoneCount()(uint32)" --rpc-url "$ETH_RPC_URL"
cast call "$ZONE_FACTORY" "verifier()(address)" --rpc-url "$ETH_RPC_URL"
```

Record the deployed address, transaction hash, and block number in the docs section for future audits.
