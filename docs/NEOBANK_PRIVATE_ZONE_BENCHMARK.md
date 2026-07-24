# Neobank private-Zone benchmark

This is the measured shape for a closed-loop private-Zone journey. It extends
the generic benchmark topology with three distinct assets and five exact
boundaries:

1. encrypted DLUSD onramp and the corresponding `EncryptedDepositProcessed`;
2. a private DLUSD transfer;
3. a composable DLUSD withdrawal to `EarnRouter`, a DLUSD/pathUSD swap,
   `EarnVault` deposit, EarnShare mint, encrypted EarnShare return, and its exact terminal
   deposit event;
4. a composable EarnShare withdrawal to the same router, `EarnVault` redemption,
   optional pathUSD/DLUSD swap, encrypted DLUSD return, and its exact terminal
   deposit event; and
5. a DLUSD off-ramp to the bridge-wallet fixture and its exact
   `WithdrawalProcessed` event.

Three focused presets reuse the same topology and correlation rules when only
one bridge direction should be loaded. `encrypted-deposit` measures the first
boundary independently. `private-withdrawal` and `swapped-redemption` prepare a
private EarnShare position outside measurement, then measure one composable
EarnShare withdrawal, vault redemption, configured swap, and encrypted DLUSD
return.

The canonical `EarnFactory`, `EarnVault`, `EarnFees`, `EarnRouter`,
`EarnContributionController`, engine, and swap adapters are vendored from the
revision in `contrib/bench/earn.lock` under
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
| `simple` (default) | Earn's canonical experimental `MinimalDirectSwapAdapter`, backed by the vendored Bridge TIP-20 controller and reserve token, installed as both DLUSD swap overrides on `EarnVault`. | The complete private swapped lifecycle that fits the current Zone callback cap. This is not a production swap venue. |
| `stablecoin-dex` | Tempo's native StablecoinDEX at tick zero through `EarnRouter`'s built-in path, with no `EarnVault` override. | Experimental native order-book comparison; the upstream private lifecycle profile has not yet proven this path end to end. |
| `direct-swap` | The full DirectSwap V2 controller, handler, authorization registry, reserve ledger, and canonical `BridgeDirectSwapAdapter`. | Contract-boundary and non-swapped-flow comparison only: the complete Bridge private callback exceeds the current Zone 10M callback cap. |

`ZONES_BENCH_SWAP_LIQUIDITY` sets the token-base-unit liquidity provisioned for
the selected path: DirectSwap reserve capacity, balances held by the simple
fixture, or bid/ask liquidity in StablecoinDEX. Deployment, approvals, pair
creation, and liquidity seeding are setup traffic outside measured latency.
Fixture metadata and the workflow report record the mechanism, adapter and
route selection, and seeded liquidity.

For `full-journey` and `swapped-lifecycle`, the minimal adapter must cover at
least `max-concurrent * withdrawal-amount` of transient inventory.
StablecoinDEX orders are consumed rather than replenished, so each
seeded side must cover `count * withdrawal-amount`. The isolated
`swapped-redemption` setup creates every account's position before measurement:
the minimal and StablecoinDEX paths therefore cover
`accounts * ceil(count/accounts) * withdrawal-amount`.
The slippage-bounce preset needs one withdrawal amount. Configuration rejects
smaller capacities, DirectSwap liquidity above the vendored controller's
`1,000,000,000,000,000` mint cap, and StablecoinDEX liquidity below its
`100,000,000` minimum order size. It rejects DirectSwap for every preset that
requires a private swap, before provisioning starts.

## Topology and policy

Provision the existing two-validator L1 plus authenticated private Zone RPC.
The local Tempo genesis supplies DLUSD and pathUSD. The neobank profile creates
EarnShare through the canonical `EarnFactory` and native TIP-20 factory, then selects the Zone token
for the requested preset: DLUSD for the full, swapped lifecycle,
swapped-redemption, slippage-bounce, encrypted-deposit, and private-withdrawal
flows, or pathUSD for the direct, third-party-recipient, and rewards redemption
flows. It deploys the vendored Earn proxy stack outside the measured interval and
enables EarnShare on the portal. The stable asset not selected by the preset
remains L1-only. The profile does not replace these assets with ordinary ERC-20
test contracts.

The profile has zero user bridge and withdrawal protocol fees. It retains a
separate generic profile with nonzero bootstrap fees. The token authorization
map must permit only the preset's selected stable asset and EarnShare in the
Zone. The only callback target is `EarnRouter` and the only terminal
off-ramp recipient is the bridge wallet fixture. No authorization map,
mnemonic, private key, encryption payload, or bearer token belongs in rendered
output or an uploaded artifact.

The Zone is created with access enforcement active and an empty allowlist,
keeping `createZone` within the production 30M L1 block limit. During untimed
fixture setup, the portal admin applies the mode-0600 file-backed benchmark
account map, adds the bridge-wallet and `EarnRouter` roles, and then enables
gateway enforcement. The temporary map is removed before measurement and is
never uploaded.

Current Zone transaction-pool admission requires a sender to hold a nonzero
balance in at least one enabled Zone token. Before untimed Zone approvals, the
runner therefore executes one 1-unit encrypted onramp per benchmark account and
waits for its exact `EncryptedDepositMade` and `EncryptedDepositProcessed`
events. This admission seed and its latency report are setup artifacts, not part
of the measured journey; preflight reserves its L1 principal and fee capacity
and then verifies every account received the balance.

The portal role model does not make the bridge wallet an exclusive plaintext
withdrawal recipient. Every admitted benchmark user has the same `Account`
role needed for deposit refunds and fallbacks, so closed access mode also
permits a plain withdrawal to any of those users. The scenario still fixes the
off-ramp destination to the bridge wallet and verifies that exact
receipt-scoped event.

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
unfunded address. `EarnRouter` callback targets, bridge-wallet off-ramps, fallback
and refund recipients, and encrypted lifecycle returns remain controlled
addresses because the scenario must spend returned assets and preserve the
closed-loop authorization policy. Generic activity, withdrawal, and roundtrip
behavior is described in `docs/ZONES_BENCHMARK.md`.

## Rendered assets

`contrib/bench/neobank/private-flow-scenario.yml` describes the complete
journey. `encrypted-deposit-scenario.yml` isolates an encrypted DLUSD portal
deposit and its exact L1 enqueue and Zone terminal events.
`swapped-redemption-position-scenario.yml` creates DLUSD-backed private
EarnShare positions outside measurement. `private-withdrawal-scenario.yml`
measures one composable EarnShare withdrawal through `EarnRouter`, vault
redemption, the selected pathUSD-to-DLUSD swap, and exact encrypted DLUSD
return.
`swapped-lifecycle-scenario.yml` isolates the encrypted DLUSD entry,
swapped Earn deposit, and swapped Earn redemption path used by the lifecycle
load test. `swapped-redemption-position-scenario.yml` creates the DLUSD-backed
private EarnShare positions outside measurement, while
`swapped-redemption-scenario.yml` measures exactly one EarnShare withdrawal,
`EarnVault` redemption, selected pathUSD-to-DLUSD callback, and encrypted
DLUSD return. `direct-lifecycle-scenario.yml` instead measures an encrypted
pathUSD entry, a pathUSD vault deposit without a DEX swap, and redemption of
the exact EarnShare amount emitted by that deposit back to pathUSD.
`third-party-recipient-scenario.yml` measures the same direct path across two
users: account A enters with pathUSD and sends the encrypted EarnShare return
to account B; B redeems the exact emitted shares and sends the encrypted
pathUSD return to A. It also measures a separate encrypted pathUSD entry for B
as the Zone fee buffer before B submits the redemption.
`slippage-bounce-scenario.yml` enters with DLUSD, submits an `EarnRouter` deposit
whose callback requires an impossible minimum vault-asset amount, observes the
failed callback and L1 bounce-back in the same receipt, and waits for the exact
terminal Zone bounce-back event. `rewards-position-scenario.yml` and
`rewards-funding-scenario.yml` prepare a private Earn position and contribute
pathUSD backing outside measurement. `rewards-redemption-scenario.yml` measures
two sequential partial redemptions and both exact encrypted pathUSD returns.
All scenarios
compose shared, receipt-correlated boundaries from
`neobank-scenario-fragments.yml`.
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
a `neobank` profile which deploys and seeds the vendored Earn stack and selected
swap mechanism outside the measured interval, enables EarnShare, sets the
bridge rates to zero, waits for Zone token ingestion, and writes only
non-secret runtime metadata:

```bash
forge build --root specs/ref-impls
export ZONES_BENCH_ENV_FILE=target/zones-benchmark/neobank-topology.env
export ZONES_BENCH_PROFILE=neobank
export ZONES_BENCH_NEOBANK_PRESET=full-journey
export ZONES_BENCH_SWAP_MECHANISM=simple
export ZONES_BENCH_SWAP_LIQUIDITY=10000000000
export ZONES_BENCH_RECIPIENT_MODE=existing
contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
```

Choose `stablecoin-dex` before `provision-topology.sh up` to run the same
scenario through the native swap path. The swap mechanism and liquidity are
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
swapped stablecoin lifecycle, `swapped-redemption` for one swapped Earn
redemption, `third-party-recipient` for the two-user direct lifecycle,
`slippage-bounce` for the failed-callback return path, `rewards-redemption` for
the rewarded private-holder redemption path, or `full-journey` for the
five-boundary journey. Use `encrypted-deposit` for the focused onramp and
`private-withdrawal` for the focused composable redemption. The selected preset
is recorded in the workflow summary and run metadata while the rendered
scenario remains at the stable results-renderer path.

The default full-journey run uses 100 accounts, 100 complete journeys, 20
journey starts per second, and at most 12 benchmark journeys in flight. The
same benchmark-side default applies to every phase and preset. It is independent
of the Zone withdrawal scheduler's
`ZONES_BENCH_WITHDRAWAL_MAX_IN_FLIGHT_BATCHES`, which limits ordered L1
withdrawal batches rather than txgen transactions or journeys. Scenario start
rates may be positive decimals, such as `1.2`, so sustained runs can target the
observed capacity without using an integer rate far above it.

The historical validation runs below predate the canonical Earn v1 fixture
refresh and used larger in-flight caps than the current default of 12. They are
retained only as a baseline for the scenario runtime; they are not current
contract-performance results. Their observed rates include ramp-up and drain.
Each linked run completed all 1,000 journeys with no failures or timeouts using
the 30,000,000 L1 gas limit and 1 GiB L1 state-bloat preset.

| Preset | Accounts | Journeys | Starts/s | Max in flight | Observed journeys/s | Journey p50 | Journey p95 | Run |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| `encrypted-deposit` | 100 | 1,000 | 40 | 100 | 36.334 | 1.505 s | 1.507 s | [29978182968](https://github.com/tempoxyz/zones/actions/runs/29978182968) |
| `slippage-bounce` | 100 | 1,000 | 4 | 100 | 1.964 | 50.662 s | 51.445 s | [29979095057](https://github.com/tempoxyz/zones/actions/runs/29979095057) |
| `rewards-redemption` | 100 | 1,000 | 2 | 100 | 0.986 | 100.875 s | 102.368 s | [29979802140](https://github.com/tempoxyz/zones/actions/runs/29979802140) |
| `swapped-lifecycle` | 100 | 1,000 | 2 | 100 | 0.984 | 100.864 s | 102.367 s | [29980825511](https://github.com/tempoxyz/zones/actions/runs/29980825511) |
| `direct-lifecycle` | 100 | 1,000 | 2 | 100 | 0.984 | 100.895 s | 102.358 s | [29981899744](https://github.com/tempoxyz/zones/actions/runs/29981899744) |
| `third-party-recipient` | 200 | 1,000 | 2 | 100 | 0.983 | 100.876 s | 102.373 s | [29982874753](https://github.com/tempoxyz/zones/actions/runs/29982874753) |
| `full-journey` | 100 | 1,000 | 2 | 100 | 0.937 | 104.863 s | 109.381 s | [29983931381](https://github.com/tempoxyz/zones/actions/runs/29983931381) |

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

This preset creates each account's DLUSD-backed private EarnShare position and
outbox allowances outside measurement. It verifies the L1 share supply and
each account's exact Zone EarnShare balance before starting the load.

Each measured journey submits a composable EarnShare withdrawal to
`EarnRouter` with the configured nonzero callback gas limit, fallback
recipient, sender tag, action ID, and encrypted return payload. Completion
requires the receipt-scoped Zone `WithdrawalRequested`, exact L1
`WithdrawalProcessed`, receipt-scoped `EarnRedeem`, and exact Zone encrypted
`DepositProcessed` for the returned DLUSD. The postcondition verifies the
remaining L1 share supply and aggregate private Zone EarnShare balance.

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

### Focused swapped-redemption load

This preset provisions a DLUSD plus EarnShare Zone and selects the requested L1
swap mechanism. Untimed setup onramps
`ceil(count/accounts) * deposit-amount` DLUSD per account and uses `EarnRouter`
deposit flow to mint
`ceil(count/accounts) * withdrawal-amount` private EarnShare per account.
Fixture deployment, both approval rounds, position creation, and its encrypted
EarnShare return are outside measured latency.

Each measured journey submits one EarnShare composable withdrawal to
`EarnRouter`. The callback redeems through `EarnVault`, swaps the returned
pathUSD to DLUSD through the selected supported swap mechanism, and
creates an encrypted DLUSD return deposit. Completion requires the exact Zone
request receipt and `WithdrawalRequested`, L1 `WithdrawalProcessed` and
`EarnRedeem` from the same receipt, and terminal Zone deposit keyed by the
router's deposit hash and the journey action ID.

Run the isolated path locally:

```bash
export ZONES_BENCH_ENV_FILE=target/zones-benchmark/swapped-redemption-topology.env
export ZONES_BENCH_PROFILE=neobank
export ZONES_BENCH_NEOBANK_PRESET=swapped-redemption
export ZONES_BENCH_SWAP_MECHANISM=simple
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
gh workflow run zones-benchmark.yml --ref '<branch-or-tag>' -f phase=neobank-swapped-redemption -f swap-mechanism=simple -f accounts=100 -f count=100 -f tps=20 -f max-concurrent=12
```

The default 12-journey in-flight cap gives a fast correctness run. A withdrawal-capacity
sweep keeps that cap, raises the requested start rate, and provisions enough
swap liquidity for every in-flight redemption. Published scenario reports use
the `neobank-swapped-redemption` results route.

For every preset with withdrawals, the workflow also scans the measured L1
block range for exact receipt-scoped `WithdrawalProcessed` events. It reports
per-block counts and process-transaction gas separately for Earn deposit
routes, Earn redemption routes, and bridge off-ramps. Earn routes include
callback success and failure counts; the intentional slippage bounce is
reported as a failed Earn-deposit callback, while a no-callback off-ramp is
marked callback-inapplicable rather than failed.

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
EarnShare topology and keep the same preset selected for the scenario runner:

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
the EarnShare return and redemption cannot be conflated with a self-transfer.
A maximum concurrency of 100 therefore requires at least 200 benchmark
accounts. Both users' pathUSD entries, the A-to-B encrypted EarnShare return,
and the B-to-A encrypted pathUSD return are inside the measured boundary;
fixture deployment, approvals, and funding remain setup traffic.

To run the slippage-bounce path independently, provision DLUSD plus EarnShare
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
recipient, token, and amount. EarnShare total supply and the engine's vault
balance must be unchanged across the measured run. The canonical terminal Zone
event does not expose the queue hash or fallback nonce, so its strongest
available correlation is the exact L1 receipt and fallback nonce followed by
the leased recipient/token/amount matcher from the pre-request Zone block.

To run the rewards-redemption path independently, provision pathUSD plus
EarnShare and select the same preset for the runner:

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
mints `N*W` private EarnShare per account, then contributes 10% of the total
`A*N*W` backing through `EarnContributionController`. Its
`fund(address,uint256,uint256)` call bounds the attempt by the exact live
EarnShare supply. The runner verifies every private position,
the unchanged share supply, and an increased `previewRedeem(W)` quote before
measurement. The measured boundary begins with the first `W/2` redemption and
ends at the exact terminal Zone encrypted-deposit event for the second
`W-W/2` redemption. Terminal checks require both L1 share supplies and the
aggregate private Zone EarnShare balance to equal `(A*N-J)*W`.

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
Bridge controller reserves, `MinimalDirectSwapAdapter`, and the
`Simple4626Vault` venue are benchmark fixtures. StablecoinDEX mode uses the
native contract through `EarnRouter`, but its tick-zero order-book liquidity is created by
provisioning. These paths do not represent final production economics,
liquidity, policy administration, or a final vault venue.

The full vendored Earn stack comes from the single revision in
`contrib/bench/earn.lock`. The vendored Zone interface contains the current
canonical `ZoneInfo.accessMode` and `ZoneInfo.gatewayMode` fields, which are
missing from that Earn revision; without those fields, `EarnRouter` would
decode `ZoneFactory.zones(zoneId)` with an obsolete tuple.
Four exact-version Bridge source pragmas are relaxed to accept the repository's
Tempo Solidity compiler. The benchmark therefore exercises the pinned contract
logic through artifacts built by the Zones Foundry project, rather than
byte-for-byte production deployment artifacts.

The canonical Bridge DirectSwap contracts and adapter are deployable for
contract-boundary and non-swapped-flow comparisons, but their complete private
swap callback exceeds Zones' current 10M callback limit. The benchmark rejects
that unsupported combination instead of reporting callback timeouts as load
results.
