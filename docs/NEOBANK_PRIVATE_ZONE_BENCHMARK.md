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

The gateway, vault adapter, rewards controller, engine, StablecoinDEX adapter, and proxy are the
Earn boundary fixtures from #750 under
`specs/ref-impls/test/fixtures/earn`. Foundry builds their artifacts alongside
the Zone specs; benchmark provisioning never fetches or clones external source.

## Topology and policy

Provision the existing two-validator L1 plus authenticated private Zone RPC.
The local Tempo genesis supplies DLUSD and pathUSD. The neobank profile creates
EarnToken through the native TIP-20 factory and selects the initial Zone token
for the requested preset: DLUSD for the full, swapped, and slippage-bounce
lifecycle flows, or pathUSD for the direct, third-party-recipient, and rewards
redemption flows. It deploys
the copied Earn proxy stack outside the measured interval and enables EarnToken
on the portal. The stable asset not selected by the preset remains L1-only. The
profile does not replace these assets with ordinary ERC-20 test contracts.

The profile has zero user bridge and withdrawal protocol fees. It retains a
separate generic profile with nonzero bootstrap fees. The token authorization
map must permit only the preset's selected stable asset and EarnToken in the
Zone. The only callback target is the gateway fixture and the only terminal
off-ramp recipient is the bridge wallet fixture. No authorization map,
mnemonic, private key, encryption payload, or bearer token belongs in rendered
output or an uploaded artifact.

Each composable request uses the exact eight-argument withdrawal overload, a
10,000,000-gas callback budget, an empty `revealTo`, an account fallback
recipient, and a random action ID used both as the withdrawal memo and callback
correlation key.
The terminal matcher requires all of: request transaction hash, sender tag,
queue/deposit hash, action ID, token, recipient, amount, and receipt-scoped
event. Balance polling is not a completion signal.

Every measured Zone withdrawal first waits at most 45 seconds for its exact
source transaction receipt and `WithdrawalRequested` event. Only a confirmed,
successful request advances to the longer cross-chain wait. This prevents a
reverted, expired, or never-included request from occupying a journey slot for
the full cross-chain timeout.

## Rendered assets

`contrib/bench/neobank/private-flow-scenario.yml` describes the complete
journey. `swapped-lifecycle-scenario.yml` isolates the encrypted DLUSD entry,
swapped Earn deposit, and swapped Earn redemption path used by the lifecycle
load test. `direct-lifecycle-scenario.yml` instead measures an encrypted pathUSD
entry, a pathUSD vault deposit without a DEX swap, and redemption of the exact
EarnToken shares emitted by that deposit back to pathUSD.
`third-party-recipient-scenario.yml` measures the same direct path across two
users: account A enters with pathUSD and sends the encrypted EarnToken return
to account B; B redeems the exact emitted shares and sends the encrypted
pathUSD return to A. It also measures a separate encrypted pathUSD entry for B
as the Zone fee buffer before B submits the redemption.
`slippage-bounce-scenario.yml` enters with DLUSD, submits a gateway deposit
whose callback requires an impossible minimum vault-asset amount, observes the
failed callback and L1 bounce-back in the same receipt, and waits for the exact
terminal Zone bounce-back event. `rewards-position-scenario.yml` and
`rewards-funding-scenario.yml` prepare a private Earn position and contribute
pathUSD backing outside measurement. `rewards-redemption-scenario.yml` measures
two sequential partial redemptions and both exact encrypted pathUSD returns.
All scenarios
compose shared, receipt-correlated boundaries from `scenario-fragments.yml`.
`l1-onramp.yml` and `zone-flow.yml` contain the underlying transaction templates
and remain separate from the generic roundtrip assets.

The transaction generator prepares each encrypted payload in memory from
the leased account, action ID, portal address, and current portal encryption
key. It ABI-encodes the canonical callback tuple directly into the composable
withdrawal `bytes` argument. Neither ciphertext nor callback data is written to
the scenario report or an artifact.

## Running the benchmark

The pinned transaction generator supports the required in-memory encrypted
deposit preparation and named-tuple ABI encoding. The topology provisioner has
a `neobank` profile which deploys and seeds the copied Earn stack and the
L1 StablecoinDEX outside the measured interval, enables EarnToken, sets the
bridge rates to zero, waits for Zone token ingestion, and writes only
non-secret runtime metadata:

```bash
forge build --root specs/ref-impls
export ZONES_BENCH_ENV_FILE=target/zones-benchmark/neobank-topology.env
export ZONES_BENCH_PROFILE=neobank
export ZONES_BENCH_NEOBANK_PRESET=direct-lifecycle
contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
```

The dedicated runner renders the profile assets, prepares account approvals and
private-RPC authorization in a mode-0700 temporary directory, invokes the
scenario, and writes the standard scenario report consumed by the existing
workflow results renderer:

```bash
contrib/bench/run-neobank-private-flow.sh
```

Set `ZONES_BENCH_NEOBANK_PRESET=swapped-lifecycle` before provisioning for the
swapped stablecoin lifecycle, `third-party-recipient` for the two-user direct
lifecycle, `slippage-bounce` for the failed-callback return path,
`rewards-redemption` for the rewarded private-holder redemption path, or
`full-journey` for the five-boundary journey. The selected preset is recorded
in the workflow summary and run metadata while the rendered scenario remains
at the stable results-renderer path.

To provision and run the direct path independently, use the same preset for
both commands so provisioning selects pathUSD as the enabled Zone stable asset:

```bash
forge build --root specs/ref-impls
export ZONES_BENCH_ENV_FILE=target/zones-benchmark/direct-topology.env
export ZONES_BENCH_PROFILE=neobank
export ZONES_BENCH_NEOBANK_PRESET=direct-lifecycle
contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
contrib/bench/run-neobank-private-flow.sh
```

To run the third-party-recipient path independently, provision its pathUSD plus
EarnToken topology and keep the same preset selected for the scenario runner:

```bash
forge build --root specs/ref-impls
export ZONES_BENCH_ENV_FILE=target/zones-benchmark/third-party-topology.env
export ZONES_BENCH_PROFILE=neobank
export ZONES_BENCH_NEOBANK_PRESET=third-party-recipient
export ZONES_BENCH_ACCOUNTS=200
contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
contrib/bench/run-neobank-private-flow.sh
```

Each third-party journey leases two distinct accounts for its full lifetime so
the EarnToken return and redemption cannot be conflated with a self-transfer.
A maximum concurrency of 100 therefore requires at least 200 benchmark
accounts. Both users' pathUSD entries, the A-to-B encrypted EarnToken return,
and the B-to-A encrypted pathUSD return are inside the measured boundary;
fixture deployment, approvals, and funding remain setup traffic.

To run the slippage-bounce path independently, provision DLUSD plus EarnToken
and keep the same preset selected for the scenario runner:

```bash
forge build --root specs/ref-impls
export ZONES_BENCH_ENV_FILE=target/zones-benchmark/slippage-bounce-topology.env
export ZONES_BENCH_PROFILE=neobank
export ZONES_BENCH_NEOBANK_PRESET=slippage-bounce
export ZONES_BENCH_ACCOUNTS=100
contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
contrib/bench/run-neobank-private-flow.sh
```

The measured success condition is the expected failure route: the request must
receive a successful Zone receipt, emit the exact `WithdrawalRequested`, reach
an L1 `WithdrawalProcessed` with `callbackSuccess=false`, emit a
`WithdrawalBounceBack` with the request's fallback nonce in that same L1
receipt, and finally emit `WithdrawalBounceBackProcessed` for the leased
recipient, token, and amount. EarnToken total supply and the engine's vault
balance must be unchanged across the measured run. The canonical terminal Zone
event does not expose the queue hash or fallback nonce, so its strongest
available correlation is the exact L1 receipt and fallback nonce followed by
the leased recipient/token/amount matcher from the pre-request Zone block.

To run the rewards-redemption path independently, provision pathUSD plus
EarnToken and select the same preset for the runner:

```bash
forge build --root specs/ref-impls
export ZONES_BENCH_ENV_FILE=target/zones-benchmark/rewards-topology.env
export ZONES_BENCH_PROFILE=neobank
export ZONES_BENCH_NEOBANK_PRESET=rewards-redemption
export ZONES_BENCH_ACCOUNTS=100
export ZONES_BENCH_COUNT=1000
export ZONES_BENCH_TPS=20
export ZONES_BENCH_MAX_CONCURRENT=100
contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
contrib/bench/run-neobank-private-flow.sh
```

For `A` accounts, `J` journeys, deposit amount `D`, and redemption amount `W`,
the runner computes `N=ceil(J/A)`. Untimed setup onramps `N*D` pathUSD and
mints `N*W` private EarnToken per account, then contributes 10% of the total
`A*N*W` backing through `VaultRewards`. It verifies every private position,
the unchanged share supply, and an increased `previewRedeem(W)` quote before
measurement. The measured boundary begins with the first `W/2` redemption and
ends at the exact terminal Zone encrypted-deposit event for the second
`W-W/2` redemption. Terminal checks require both L1 share supplies and the
aggregate private Zone EarnToken balance to equal `(A*N-J)*W`.

The runner rejects reward configurations whose pathUSD cannot cover the
position plus worst-case setup and measured transaction fee caps. Position
setup and reward funding write separate reports; only the 18-step two-redeem
scenario contributes benchmark latency and throughput.

That runner should provision the existing topology, deploy and configure the
fixtures outside measurement, create the private-RPC authorization map in a
mode-0700 temporary directory, run the scenario, publish per-step and journey
latency, and remove the temporary material on exit.

## Production differences

The topology exercises real L1 and Zone nodes, portal deposits, outbox batches,
authenticated private RPC, and receipt-scoped cross-chain correlation. The
StablecoinDEX liquidity and `Simple4626Vault` venue are benchmark fixtures.
They do not represent final production economics, liquidity, policy
administration, or a final vault venue.

The checked-in rewards fixture is ABI-consistent with its pinned Earn stack and
uses `fund(address,uint256)`. Current upstream Earn adds a `maxShareSupply`
argument. This benchmark intentionally does not mix that newer interface with
the pinned gateway, adapter, and rewards bytecode; the missing supply guard is
a known production difference.

The pinned `VaultAdapter` uses `redeem(uint256,address)`, and its gateway checks
`minVaultAssets` after the adapter returns. Current upstream forwards that
minimum into `redeem(uint256,address,uint256)` so the adapter enforces it. The
benchmark keeps the gateway and adapter from the same pinned stack; this
minimum-assets enforcement boundary is another known production difference.
