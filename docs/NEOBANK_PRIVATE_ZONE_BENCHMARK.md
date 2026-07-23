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

Two focused presets reuse the same topology and correlation rules when only
one bridge direction should be loaded. `encrypted-deposit` measures the first
boundary independently. `private-withdrawal` prepares portal-backed private
DLUSD outside measurement, then measures the fifth boundary independently.

The gateway, vault adapter, rewards controller, engine, swap adapters, and proxy
are Earn boundary fixtures from #750 and #809 under
`specs/ref-impls/test/fixtures/earn`. Foundry builds their artifacts alongside
the Zone specs; benchmark provisioning never fetches or clones external
source. These assets, the generic topology, scenario runtime, reporting, and CI
workflow are one main-based benchmark implementation rather than a stack of
benchmark branches.

### Swap mechanism

`ZONES_BENCH_SWAP_MECHANISM` selects the L1 swap path during fixture
provisioning:

| Value | Provisioned path | Intended comparison |
| --- | --- | --- |
| `direct-swap` (default) | The full DirectSwap V2 controller, handler, authorization registry, TIP-20 adapter, reserve ledger, and canonical bridge swap adapter. The gateway installs explicit DLUSD deposit and redeem routes to this adapter. | The complete checked-in DirectSwap boundary fixture. |
| `simple` | A benchmark-only, funded 1:1 implementation of the canonical three-argument direct-swap interface, wrapped by the canonical bridge swap adapter and installed as the DLUSD route override. | Callback and gateway cost without the full DirectSwap controller stack. This is not a production swap venue. |
| `stablecoin-dex` | Tempo's native StablecoinDEX at tick zero through the gateway's default StablecoinDEX adapter, with no per-token route override. | The native order-book path and default gateway routing. |

`ZONES_BENCH_SWAP_LIQUIDITY` sets the token-base-unit liquidity provisioned for
the selected path: DirectSwap reserve capacity, balances held by the simple
fixture, or bid/ask liquidity in StablecoinDEX. Deployment, approvals, pair
creation, and liquidity seeding are setup traffic outside measured latency.
Fixture metadata and the workflow report record the mechanism, adapter and
route selection, and seeded liquidity.

For `full-journey` and `swapped-lifecycle`, DirectSwap and the simple fixture
must cover at least `max-concurrent * withdrawal-amount` of transient inventory.
StablecoinDEX orders are consumed rather than replenished, so each seeded side
must cover `count * withdrawal-amount`. The slippage-bounce preset needs one
withdrawal amount. Configuration rejects smaller capacities, DirectSwap
liquidity above the pinned controller's `1,000,000,000,000,000` mint cap, and
StablecoinDEX liquidity below its `100,000,000` minimum order size.

## Topology and policy

Provision the existing two-validator L1 plus authenticated private Zone RPC.
The local Tempo genesis supplies DLUSD and pathUSD. The neobank profile creates
EarnToken through the native TIP-20 factory and selects the initial Zone token
for the requested preset: DLUSD for the full, swapped, and slippage-bounce
lifecycle flows, encrypted-deposit, and private-withdrawal, or pathUSD for the
direct, third-party-recipient, and rewards redemption flows. It deploys
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

Each composable request uses the exact eight-argument withdrawal overload, the
configured callback budget (`10,000,000` gas by default), an empty `revealTo`,
an account fallback recipient, and a random action ID used both as the
withdrawal memo and callback correlation key.
The terminal matcher requires all of: request transaction hash, sender tag,
queue/deposit hash, action ID, token, recipient, amount, and receipt-scoped
event. Balance polling is not a completion signal.

Every measured Zone withdrawal first waits at most 45 seconds for its exact
source transaction receipt and `WithdrawalRequested` event. Only a confirmed,
successful request advances to the longer cross-chain wait. This prevents a
reverted, expired, or never-included request from occupying a journey slot for
the full cross-chain timeout.

`ZONES_BENCH_RECIPIENT_MODE=existing|random` controls destination reuse. In the
closed-loop neobank journey it changes only the ordinary private DLUSD transfer:
`existing` reuses a benchmark account and `random` targets a fresh, seeded
unfunded address. Gateway callback targets, bridge-wallet off-ramps, fallback
and refund recipients, and encrypted lifecycle returns remain controlled
addresses because the scenario must spend returned assets and preserve the
closed-loop authorization policy. Generic activity, withdrawal, and roundtrip
behavior is described in `docs/ZONES_BENCHMARK.md`.

## Rendered assets

`contrib/bench/neobank/private-flow-scenario.yml` describes the complete
journey. `encrypted-deposit-scenario.yml` isolates an encrypted DLUSD portal
deposit and its exact L1 enqueue and Zone terminal events.
`private-withdrawal-funding-scenario.yml` creates portal-backed private DLUSD
outside measurement, while `private-withdrawal-scenario.yml` measures the
corresponding outbox request and exact L1 terminal event.
`swapped-lifecycle-scenario.yml` isolates the encrypted DLUSD entry,
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
a `neobank` profile which deploys and seeds the copied Earn stack and selected
swap mechanism outside the measured interval, enables EarnToken, sets the
bridge rates to zero, waits for Zone token ingestion, and writes only
non-secret runtime metadata:

```bash
forge build --root specs/ref-impls
export ZONES_BENCH_ENV_FILE=target/zones-benchmark/neobank-topology.env
export ZONES_BENCH_PROFILE=neobank
export ZONES_BENCH_NEOBANK_PRESET=full-journey
export ZONES_BENCH_SWAP_MECHANISM=direct-swap
export ZONES_BENCH_SWAP_LIQUIDITY=10000000000
export ZONES_BENCH_RECIPIENT_MODE=existing
contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
```

Choose `simple` or `stablecoin-dex` before `provision-topology.sh up` to run the
same scenario through another swap path. The swap mechanism and liquidity are
deployment-time settings, so an A/B comparison creates a fresh topology for
each path. Recipient mode is a render-time setting and can change between runs
against a compatible topology.

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
`full-journey` for the five-boundary journey. Use `encrypted-deposit` for the
focused onramp and `private-withdrawal` for the focused off-ramp. The selected
preset is recorded in the workflow summary and run metadata while the rendered
scenario remains at the stable results-renderer path.

The default full-journey run uses 100 accounts, 1,000 complete journeys, 20
journey starts per second, and at most 12 benchmark journeys in flight. The
same benchmark-side default applies to every phase and preset. It is independent
of the Zone withdrawal scheduler's
`ZONES_BENCH_WITHDRAWAL_MAX_IN_FLIGHT_BATCHES`, which limits ordered L1
withdrawal batches rather than txgen transactions or journeys.

The historical validation runs below used larger in-flight caps than the
current default of 12. Their observed rates include ramp-up and drain. Each
linked run completed all 1,000 journeys with no failures or timeouts using the
30,000,000 L1 gas limit and 1 GiB L1 state-bloat preset, and published its
report to ClickHouse.

| Preset | Accounts | Journeys | Starts/s | Max in flight | Observed journeys/s | Journey p50 | Journey p95 | Run |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| [`encrypted-deposit`](https://pr-127-tempo-apps-internal-perf.tempo-dev.workers.dev/scenarios/neobank-encrypted-deposit) | 100 | 1,000 | 40 | 100 | 36.334 | 1.505 s | 1.507 s | [29978182968](https://github.com/tempoxyz/zones/actions/runs/29978182968) |
| [`private-withdrawal`](https://pr-127-tempo-apps-internal-perf.tempo-dev.workers.dev/scenarios/neobank-private-withdrawal) | 200 | 1,000 | 30 | 200 | 12.744 | 12.037 s | 28.418 s | [29978430968](https://github.com/tempoxyz/zones/actions/runs/29978430968) |
| [`slippage-bounce`](https://pr-127-tempo-apps-internal-perf.tempo-dev.workers.dev/scenarios/neobank-slippage-bounce) | 100 | 1,000 | 4 | 100 | 1.964 | 50.662 s | 51.445 s | [29979095057](https://github.com/tempoxyz/zones/actions/runs/29979095057) |
| [`rewards-redemption`](https://pr-127-tempo-apps-internal-perf.tempo-dev.workers.dev/scenarios/neobank-rewards-redemption) | 100 | 1,000 | 2 | 100 | 0.986 | 100.875 s | 102.368 s | [29979802140](https://github.com/tempoxyz/zones/actions/runs/29979802140) |
| [`swapped-lifecycle`](https://pr-127-tempo-apps-internal-perf.tempo-dev.workers.dev/scenarios/neobank-swapped-lifecycle) | 100 | 1,000 | 2 | 100 | 0.984 | 100.864 s | 102.367 s | [29980825511](https://github.com/tempoxyz/zones/actions/runs/29980825511) |
| [`direct-lifecycle`](https://pr-127-tempo-apps-internal-perf.tempo-dev.workers.dev/scenarios/neobank-direct-lifecycle) | 100 | 1,000 | 2 | 100 | 0.984 | 100.895 s | 102.358 s | [29981899744](https://github.com/tempoxyz/zones/actions/runs/29981899744) |
| [`third-party-recipient`](https://pr-127-tempo-apps-internal-perf.tempo-dev.workers.dev/scenarios/neobank-third-party-recipient) | 200 | 1,000 | 2 | 100 | 0.983 | 100.876 s | 102.373 s | [29982874753](https://github.com/tempoxyz/zones/actions/runs/29982874753) |
| [`full-journey`](https://pr-127-tempo-apps-internal-perf.tempo-dev.workers.dev/scenarios/neobank-private-zone-flow) | 100 | 1,000 | 2 | 100 | 0.937 | 104.863 s | 109.381 s | [29983931381](https://github.com/tempoxyz/zones/actions/runs/29983931381) |

### Focused encrypted-deposit load

The measured boundary starts at the per-journey Zone checkpoint and in-memory
encryption preparation, submits `ZonePortal.depositEncrypted` on L1, matches the
receipt-scoped `EncryptedDepositMade`, and ends at the exact
`EncryptedDepositProcessed` in the Zone. The matcher ties the terminal event to
the L1 deposit hash, sender, recipient, DLUSD address, net amount, and random
action ID. Portal approval is setup traffic and is excluded from latency and
throughput.

Run the focused preset locally against a freshly provisioned neobank topology:

```bash
export ZONES_BENCH_ENV_FILE=target/zones-benchmark/encrypted-deposit-topology.env
export ZONES_BENCH_PROFILE=neobank
export ZONES_BENCH_NEOBANK_PRESET=encrypted-deposit
export ZONES_BENCH_ACCOUNTS=100
export ZONES_BENCH_COUNT=100
export ZONES_BENCH_TPS=20
export ZONES_BENCH_MAX_CONCURRENT=12
contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
contrib/bench/run-neobank-private-flow.sh
```

After the workflow reaches the default branch, the equivalent one-command CI
invocation is:

```bash
gh workflow run zones-benchmark.yml --ref '<branch-or-tag>' -f phase=neobank-encrypted-deposit -f accounts=100 -f count=100 -f tps=20 -f max-concurrent=12
```

Published scenario reports use the `neobank-encrypted-deposit` results route.

### Focused private-withdrawal load

This preset first deposits `ceil(count/accounts) * deposit-amount` DLUSD to
each benchmark account through the encrypted portal path. That funding run,
portal approval, and user outbox approval are setup traffic. The runner waits
for the exact terminal Zone deposit events, then waits up to two minutes for
the portal's processed-deposit counter to reach the recorded deposit count. It
verifies that each account can cover every withdrawal plus its worst-case Zone
transaction fee cap and already has the required outbox allowance before it
starts measurement.

Each measured journey checkpoints L1, submits the exact eight-argument outbox
withdrawal with `gasLimit=0`, empty callback data, empty `revealTo`, the leased
account as fallback recipient, and a random action ID as memo. It then requires
the successful Zone receipt, its receipt-scoped `WithdrawalRequested`, and the
exact L1 `WithdrawalProcessed` for the Bridge wallet. The terminal matcher uses
the sender tag derived from the Zone sender and request transaction hash, plus
the DLUSD token, amount, recipient, and successful callback flag. The runner
also verifies the Bridge wallet's aggregate L1 DLUSD increase after the run.

Run it locally with the same fast defaults:

```bash
export ZONES_BENCH_ENV_FILE=target/zones-benchmark/private-withdrawal-topology.env
export ZONES_BENCH_PROFILE=neobank
export ZONES_BENCH_NEOBANK_PRESET=private-withdrawal
export ZONES_BENCH_ACCOUNTS=100
export ZONES_BENCH_COUNT=100
export ZONES_BENCH_TPS=20
export ZONES_BENCH_MAX_CONCURRENT=12
contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
contrib/bench/run-neobank-private-flow.sh
```

The equivalent one-command CI invocation is:

```bash
gh workflow run zones-benchmark.yml --ref '<branch-or-tag>' -f phase=neobank-private-withdrawal -f accounts=100 -f count=100 -f tps=20 -f max-concurrent=12
```

Published scenario reports use the `neobank-private-withdrawal` results route.

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
export ZONES_BENCH_MAX_CONCURRENT=12
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
Bridge controller reserves, the funded simple swap, and the `Simple4626Vault`
venue are benchmark fixtures. StablecoinDEX mode uses the native contract and
canonical adapter, but its tick-zero order-book liquidity is created by
provisioning. These paths do not represent final production economics,
liquidity, policy administration, or a final vault venue.

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
