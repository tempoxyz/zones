# Neobank private-Zone benchmark

This is the measured shape for a closed-loop private-Zone journey. It extends
the generic benchmark topology with three distinct assets and five exact
boundaries:

1. encrypted DLUSD onramp and the corresponding `EncryptedDepositProcessed`;
2. a private DLUSD transfer;
3. a composable DLUSD withdrawal to the gateway, a DLUSD/pathUSD swap, vault
   deposit, EarnToken mint, encrypted EarnToken return, and its exact terminal
   deposit event;
4. a composable EarnToken withdrawal to the same gateway, vault redemption,
   optional pathUSD/DLUSD swap, encrypted DLUSD return, and its exact terminal
   deposit event; and
5. a DLUSD off-ramp to the bridge-wallet fixture and its exact
   `WithdrawalProcessed` event.

The gateway, vault adapter, engine, Earn factory, and bridge swap adapter are
built from the exact `tempoxyz/earn` revision in
[`contrib/bench/earn.lock`](../contrib/bench/earn.lock). The workflow verifies
that revision after checkout and builds it with that repository's Foundry
configuration. Zones runtime code consumes only the resulting artifacts; it
does not fetch or clone external source.

## Topology and policy

Provision the existing two-validator L1 plus authenticated private Zone RPC.
The local Tempo genesis supplies DLUSD and pathUSD. The neobank profile makes
DLUSD the initial Zone token, deploys the pinned Earn factory stack outside the
measured interval, enables the factory-created EarnToken on the portal, and
keeps pathUSD as L1-only vault collateral. It does not replace those assets
with ordinary ERC-20 test contracts.

The profile has zero user bridge and withdrawal protocol fees. It retains a
separate generic profile with nonzero bootstrap fees. The token authorization
map must permit only DLUSD and EarnToken in the Zone. The only callback target
is the gateway fixture and the only terminal off-ramp recipient is the bridge
wallet fixture. No authorization map, mnemonic, private key, encryption
payload, or bearer token belongs in rendered output or an uploaded artifact.

Each composable request uses the exact eight-argument withdrawal overload,
nonzero callback gas, an empty `revealTo`, an account fallback recipient, and a
random action ID used both as the withdrawal memo and callback correlation key.
The terminal matcher requires all of: request transaction hash, sender tag,
queue/deposit hash, action ID, token, recipient, amount, and receipt-scoped
event. Balance polling is not a completion signal.

## Rendered assets

`contrib/bench/neobank/private-flow-scenario.yml` describes the complete
journey. `l1-onramp.yml` contains the encrypted L1 entry; `zone-flow.yml`
contains the private transfer, both composable requests, and the off-ramp.
They are intentionally separate from the generic roundtrip assets.

The transaction generator prepares all three encrypted payloads in memory from
the leased account, action ID, portal address, and current portal encryption
key. It ABI-encodes the canonical callback tuple directly into the composable
withdrawal `bytes` argument. Neither ciphertext nor callback data is written to
the scenario report or an artifact.

## Current blocking capability

The pinned transaction generator supports the required in-memory encrypted
deposit preparation and named-tuple ABI encoding. The topology provisioner has
a `neobank` profile which deploys and seeds the L1 stack outside the measured
interval, enables the factory-created EarnToken, sets the bridge rates to zero,
waits for Zone token ingestion, and writes only non-secret runtime metadata.
Provide artifacts built from the pinned external source when running locally:

```bash
earn_root=/path/to/earn-at-"$(sed -n 's/^EARN_REV=//p' contrib/bench/earn.lock)"
forge build --root "$earn_root"
forge build --root "$earn_root" "$earn_root/localnet/foundry/src/LocalDirectSwap.sol"
ZONES_BENCH_PROFILE=neobank \
ZONES_BENCH_EARN_ARTIFACTS="$earn_root/out" \
ZONES_BENCH_EARN_LOCALNET_ARTIFACTS="$earn_root/out" \
contrib/bench/provision-topology.sh up
```

The dedicated runner renders the profile assets, prepares account approvals and
private-RPC authorization in a mode-0700 temporary directory, invokes the
scenario, and writes the standard scenario report consumed by the existing
workflow results renderer:

```bash
contrib/bench/run-neobank-private-flow.sh
```

That runner should provision the existing topology, deploy and configure the
fixtures outside measurement, create the private-RPC authorization map in a
mode-0700 temporary directory, run the scenario, publish per-step and journey
latency, and remove the temporary material on exit.

## Production differences

The topology exercises real L1 and Zone nodes, portal deposits, outbox batches,
authenticated private RPC, and receipt-scoped cross-chain correlation. The
direct-swap liquidity and `Simple4626Vault` venue are pinned-source benchmark
fixtures with 1:1 behavior. They do not represent final production economics,
liquidity, policy administration, or a final vault venue. The external
bytecode used here is not asserted to be the final production stack.
