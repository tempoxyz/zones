# Zones benchmark transaction generation

This benchmark support prepares three independent `txgen-tempo` workloads for an
L1 deposit, ordinary Zone activity, and a Zone withdrawal. It does not submit a
cross-chain scenario, wait for deposit or withdrawal events, or decide when the
next phase is ready.

## Production-shaped benchmark environment

Do not use Anvil or `tempo-zone dev` for performance results. The checked-in
provisioner follows Tempo's e2e benchmark shape and creates the benchmark target
on the runner itself:

1. `tempo-xtask generate-localnet` creates a two-validator Tempo genesis with
   DKG material and an explicitly supplied private mnemonic.
2. `tempo-xtask install-reference-zone-factory` installs constructor-equivalent
   reference ZoneFactory, Verifier, and ZoneMessenger state at the canonical
   TIP-1091 factory address before either validator database is initialized.
3. Two real `tempo node` consensus processes start from separate databases and
   identities. The provisioner waits for both RPCs, peering, and chain progress.
4. The factory owner submits a real `createZone` transaction. A separate portal
   admin controls the Zone, and a separate hot sequencer registers its encryption
   key on L1.
5. A release-equivalent `tempo-zone node --sequencer` starts from the generated
   Zone genesis and a durable datadir. The provisioner waits for Zone block
   production and for its finalized L1 view to contain the portal configuration.

The topology reserves mnemonic indices 0 through 4: index 0 is the factory
owner, indices 1 and 2 are the Tempo validator identities, index 3 is the portal
admin, and index 4 is the sequencer. The benchmark account pool starts at index
16 in the workflow. The generated Tempo genesis creates pathUSD, AlphaUSD,
BetaUSD, and ThetaUSD and funds every generated account, so benchmark accounts
and the sequencer have L1 token balances without a faucet. The topology
environment file contains addresses and RPC metadata but never the mnemonic or
private keys.

The default workflow dispatch uses Tempo's `bloat=100` preset: 100,000 MiB of
storage across all four TIP-20s, imported identically into both Tempo databases.
On the 32-logical-CPU benchmark runner, the default partition is:

| Process | CPUs | HTTP RPC | Metrics |
| --- | --- | --- | --- |
| Tempo validator A | `0-3,16-19` | `127.0.0.1:8545` | `127.0.0.1:9001` |
| Tempo validator B | `4-7,20-23` | `127.0.0.1:8645` | `127.0.0.1:9101` |
| Zone sequencer | `8-13,24-29` | `127.0.0.1:8546` | `127.0.0.1:9201` |
| txgen and bench | `14-15,30-31` | n/a | n/a |

The two Tempo databases use separate restored Schelk volumes. The Zone datadir
shares validator A's volume but not its database. The provisioner accepts
`ZONES_BENCH_L1_A_CPUS`, `ZONES_BENCH_L1_B_CPUS`, and
`ZONES_BENCH_ZONE_CPUS` overrides when a runner has a different CPU topology.
The pinned Tempo harness uses a `1,000,000,000,000` L1 gas limit; the provisioner
sets that value in both genesis and each validator's payload builder. Override
`ZONES_BENCH_L1_GAS_LIMIT` only when deliberately testing another Tempo setup.
The temporary bloat dump is also placed on validator A's benchmark volume, not
the runner's root filesystem; `ZONES_BENCH_BLOAT_TMP_DIR` can override it.

`bench send` uses `txpool_status` for its default drain check. The selected
trusted RPC must expose the `txpool` module, or the benchmark must explicitly use
a zero drain timeout. Every generated workload still waits for its transaction
receipt, but zero skips the final global pool-empty check and weakens isolation
between consecutive runs. The workflow and runner script validate
`txpool_status` before sending when the drain timeout is non-zero.

This is production-shaped rather than production-equivalent. Zones is currently
documented as testnet-only, the optional multi-sequencer topology has a static
leader with no automatic promotion, and proof generation is not final. The
reference factory deploys full portal bytecode, while the proposed native
TIP-1091 lifecycle uses a protocol-managed implementation, so portal deployment
gas is not final-production exact.

### Fresh fixture boundary

The provisioner stops at infrastructure readiness. It does not make benchmark
deposits, wait for deposit ingestion, or create a cross-chain snapshot. A fresh
topology can therefore run the deposit phase. It cannot honestly run activity
or withdrawal: those phases require Zone balances created by real deposits and
backed by portal escrow.

The phase runner makes that boundary explicit with preflight fixture assertions:

- `--fixture-state empty`, used for deposit, requires zero recorded and processed
  deposits, a zero portal token balance, and zero Zone balance for every account
  in the benchmark pool.
- `--fixture-state funded`, used for activity and withdrawal, requires at least
  one recorded deposit, all recorded deposits processed, a nonzero aggregate
  Zone balance for the pool, and portal escrow at least as large as that balance.
  The ordinary phase-specific balance and allowance checks still apply to every
  account.

Consequently, the workflow rejects an activity or withdrawal request in its
configuration job before reserving the bare-metal runner. Making those phases
runnable requires a later fixture builder that performs untimed real deposits,
waits for ingestion, and captures a coordinated L1/Zone state fixture, or an
equivalent externally prepared fixture. Neither path exists in this change, and
unbacked Zone genesis balances are not a valid substitute.

### Provision and stop the topology manually

Build the Zone and pinned Tempo binaries in an optimized profile, then pass
their exact paths to the provisioner. To match Tempo's `bloat=100` preset in a
manual run, set the bloat size to 100000 MiB; direct script invocation defaults to zero so a
developer can perform a topology smoke test without creating a 100 GiB dump.

```bash
export TEMPO_ROOT="$HOME/projects/tempo"

cargo build --profile maxperf -p tempo-zone -p tempo-xtask
cargo build --manifest-path "$TEMPO_ROOT/Cargo.toml" \
  --profile maxperf --bin tempo
cargo build --manifest-path "$TEMPO_ROOT/Cargo.toml" \
  --profile maxperf -p tempo-xtask

export ZONE_BIN="$PWD/target/maxperf/tempo-zone"
export ZONES_XTASK_BIN="$PWD/target/maxperf/tempo-xtask"
export TEMPO_BIN="$TEMPO_ROOT/target/maxperf/tempo"
export TEMPO_XTASK_BIN="$TEMPO_ROOT/target/maxperf/tempo-xtask"
export ZONES_BENCH_MNEMONIC='<private mnemonic for this isolated run>'
export ZONES_BENCH_ACCOUNT_START=16
export ZONES_BENCH_ACCOUNTS=100
export ZONES_BENCH_BLOAT_MIB=100000
export ZONES_BENCH_TOPOLOGY_DIR="$PWD/target/zones-benchmark/topology"
export ZONES_BENCH_STATE_A_ROOT='/reth-bench-a/zones-manual-<unique-run-id>'
export ZONES_BENCH_STATE_B_ROOT='/reth-bench-b/zones-manual-<unique-run-id>'
export ZONES_BENCH_ENV_FILE="$PWD/target/zones-benchmark/topology.env"
export ZONES_BENCH_PID_FILE="$PWD/target/zones-benchmark/topology.pids"

contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
```

`up` leaves all three node processes alive. It also configures nonzero portal
deposit and bounce-back fee rates before declaring the topology ready. The
withdrawal rate cannot be configured on a fresh Zone because that transaction
needs Zone fee token. After real deposits fund the sequencer, a future fixture
builder must run the following untimed setup action before capturing the funded
fixture:

```bash
SEQUENCER_KEY='<private sequencer key>' \
  "$ZONES_XTASK_BIN" configure-benchmark-fees \
  --l1-rpc-url "$L1_RPC_URL" \
  --portal "$L1_PORTAL_ADDRESS" \
  --zone-rpc-url "$ZONE_RPC_URL" \
  --tempo-gas-rate 1
```

Always stop the recorded processes with the matching PID file:

```bash
contrib/bench/provision-topology.sh cleanup "$ZONES_BENCH_PID_FILE"
```

The provisioner intentionally rejects existing topology and database paths. Use
new paths for another run, or remove disposable paths only after cleanup.

Tempo's current `generate-localnet` and `generate-state-bloat` commands accept a
mnemonic only as a command-line value. The provisioner does not print commands,
enables no shell tracing, masks the value in GitHub Actions, and excludes it from
rendered artifacts, but the mnemonic is transiently visible to same-host process
inspection while those Tempo commands execute. Eliminating that exposure needs
an upstream Tempo environment- or file-based mnemonic option.

## What the benchmark xtask does

`tempo-xtask benchmark-preflight` is a read-only configuration step. It:

- derives the configured account addresses without printing or serializing the
  mnemonic;
- queries both chain IDs rather than assuming them, checks configured deployment
  expectations, and verifies the Zone chain ID derived from the portal's Zone
  ID;
- records both client versions and genesis hashes in `preflight.json`;
- resolves and cross-checks the portal, outbox, and enabled token;
- queries deposit, bounce-back, and withdrawal fees and deposit status;
- records both RPC gas-price estimates and floors the default Zone fee caps at
  Tempo's nonzero T0 base fee when a fresh Zone reports a zero estimate;
- checks token balances and allowances for every benchmark account;
- rejects phase amounts that do not cover protocol and transaction fee budgets;
- renders all three txgen specs and injects any required untimed approvals; and
- writes a non-secret `preflight.json` report.

It does not create a Zone, fund an account, submit an approval or workload
transaction, wait for cross-chain state, or run txgen. The workflow calls it
only after the separate topology provisioner has created and started the Zone.

## Prerequisites

Install compatible `txgen-tempo` and `bench` binaries from an explicitly pinned
revision of the public `tempoxyz/txgen` repository:

```bash
export TXGEN_REV='<approved txgen commit>'
cargo install --git https://github.com/tempoxyz/txgen \
  --rev "$TXGEN_REV" --locked txgen-tempo bench-cli
```

The render/generation compatibility test can be run with:

```bash
TXGEN_TEMPO_BIN="$(command -v txgen-tempo)" \
  cargo test -p tempo-xtask \
  txgen_generates_representative_local_transactions_when_installed
```

For manual phase generation, keep the same private mnemonic used to provision
the isolated network in the environment and load the provisioner's non-secret
RPC/address output:

```bash
export ZONES_BENCH_MNEMONIC='your secret benchmark mnemonic'
source target/zones-benchmark/topology.env
```

There is no fallback mnemonic or public write endpoint. The provisioner funds
the derived pool in the isolated Tempo genesis. Use real deposits to prepare the
corresponding Zone balances before the activity or withdrawal phase; preflight
fails instead of silently funding or bridging an account.

## Run preflight

Run all checks and render all workload specs with:

```bash
cargo run -p tempo-xtask -- benchmark-preflight \
  --l1-rpc-url "$L1_RPC_URL" \
  --zone-rpc-url "$ZONE_RPC_URL" \
  --token "$ZONES_BENCH_TOKEN" \
  --accounts 100 \
  --deposit-amount 1000000 \
  --activity-amount 1 \
  --withdrawal-amount 1000000 \
  --transactions-per-account 100 \
  --check-phase deposit \
  --fixture-state empty \
  --output target/zones-benchmark
```

Pass `--zone-dir generated/<zone>` when generated Zone metadata should be used
to resolve and cross-check the deployed addresses. Use `--check-phase deposit`
with `--fixture-state empty` for the fresh topology. A separately prepared real
deposit fixture uses `--check-phase activity` or `withdrawal` with
`--fixture-state funded`. Preflight always queries and reports both networks and
renders all three specs, but injects approval setup only for the selected phase.
The output directory contains `deposit.yml`, `zone-activity.yml`,
`withdrawal.yml`, `preflight.json`, and the `abis/` files referenced by the
specs.

Set `--transactions-per-account` to a conservative upper bound for how many
measured transactions any one account may send. Preflight uses that capacity
when checking token balances, transaction-fee headroom, and allowances; its
default is one. The current txgen pool supports random or fixed-index selection,
not round-robin selection, so no per-account maximum below the full transaction
count is guaranteed. The phase runner therefore budgets the full count for every
account. A lower manual value is a probabilistic capacity assumption. For
expiring Zone activity, preflight also budgets txgen's monotonic fee-cap
uniqueness bump through the configured transaction capacity.

When an approval is needed, the rendered deposit or withdrawal spec includes it
as a txgen setup transaction. `bench send` submits setup transactions and waits
for their inclusion before it starts workload timing. Keep `--rpc` on the
`txgen-tempo generate` command so these regular-nonce setup transactions start
from the account's current protocol nonce. Use one write RPC for a phase that
contains setup approvals.

## Run one phase

`contrib/bench/run-phase.sh` combines preflight, spec selection, generation, and
submission for exactly one phase. It does not run a phase before or after the
selected one.

```bash
export ZONES_BENCH_SEED='<unique unsigned integer>'
export ZONES_BENCH_ACCOUNTS=100
export ZONES_BENCH_COUNT=1000
export ZONES_BENCH_TPS=100
export ZONES_BENCH_MAX_CONCURRENT=100

contrib/bench/run-phase.sh deposit
# or: contrib/bench/run-phase.sh activity
# or: contrib/bench/run-phase.sh withdrawal
```

Only `deposit` is valid immediately after `provision-topology.sh up`.
`activity` and `withdrawal` are independent entry points for a later, verified
funded fixture; they are not a phase chain.

Use a different seed for each run. A fixed seed repeats correlation memos across
runs, even though memos remain distinct within one generated stream. The runner
requires every account to have enough balance for the full count because sender
selection is random.

### Tempo L1 deposit

The deposit workload calls
`deposit(address,address,uint128,bytes32,address)`. It deposits to the selected
sender's corresponding address in the same pool and generates a random
`bytes32` memo. Any portal approvals are setup traffic and are not included in
the measured workload.

To run the pipeline manually:

```bash
txgen-tempo generate \
  --spec target/zones-benchmark/deposit.yml \
  --count "$ZONES_BENCH_COUNT" \
  --seed "$ZONES_BENCH_SEED" \
  --rpc "$L1_RPC_URL" \
| bench send \
  --rpc-url "$L1_RPC_URL" \
  --tps "$ZONES_BENCH_TPS" \
  --max-concurrent "$ZONES_BENCH_MAX_CONCURRENT"
```

### Zone activity

The rendered spec uses the queried Zone chain ID and pays gas in the configured
enabled TIP-20. Activity transactions use expiring nonces with a 25-second
validity window, so they must be streamed into `bench send`; do not generate a
file for later replay.

```bash
txgen-tempo generate \
  --spec target/zones-benchmark/zone-activity.yml \
  --count "$ZONES_BENCH_COUNT" \
  --seed "$ZONES_BENCH_SEED" \
  --rpc "$ZONE_RPC_URL" \
| bench send \
  --rpc-url "$ZONE_RPC_URL" \
  --tps "$ZONES_BENCH_TPS" \
  --max-concurrent "$ZONES_BENCH_MAX_CONCURRENT"
```

### Zone withdrawal

The workload calls
`requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes,bytes)`
with `gasLimit = 0`, empty callback data, and empty `revealTo`. The Tempo
recipient and Zone fallback recipient are the selected sender's corresponding
account address. Any outbox approvals are untimed setup traffic.

```bash
txgen-tempo generate \
  --spec target/zones-benchmark/withdrawal.yml \
  --count "$ZONES_BENCH_COUNT" \
  --seed "$ZONES_BENCH_SEED" \
  --rpc "$ZONE_RPC_URL" \
| bench send \
  --rpc-url "$ZONE_RPC_URL" \
  --tps "$ZONES_BENCH_TPS" \
  --max-concurrent "$ZONES_BENCH_MAX_CONCURRENT"
```

## GitHub workflow

`.github/workflows/zones-benchmark.yml` runs one selected phase on the same
private `bare-metal-dual-schelk` runner class used by Tempo's e2e benchmark. It
does not use a GitHub Actions environment, a pre-existing Zone, or configured
write RPCs. Each job:

1. restores the two isolated Schelk volumes and assigns unique state roots;
2. checks out exact Tempo and txgen revisions and builds max-performance Tempo
   and Zone binaries;
3. generates a private 24-word mnemonic for the run unless the optional
   `ZONES_BENCH_MNEMONIC` repository secret is set;
4. generates the two-validator L1, optionally imports the requested four-token
   state bloat into both databases, creates the Zone, and starts all nodes;
5. runs one preflight/generate/send phase; and
6. uploads the rendered files, report, and node logs before stopping the nodes
   and restoring the benchmark volumes.

Tempo and txgen are checked out from their public repositories at exact commits,
so the workflow requires no dependency-access secret. No benchmark mnemonic,
portal, chain ID, token, target ID, RPC URL, or metrics URL needs to be configured
externally. Repository or organization Actions administrators must make the
`bare-metal-dual-schelk` runner label available to this repository.

After the workflow exists on the default branch, dispatch it from the Actions
UI or CLI and select the branch/ref to test:

```bash
gh workflow run zones-benchmark.yml \
  --ref '<branch-or-tag>' \
  -f phase=deposit \
  -f accounts=100 \
  -f count=1000 \
  -f tps=100 \
  -f state-bloat-gib=100
```

GitHub does not expose a newly introduced `workflow_dispatch` until the workflow
file exists on the default branch. For a same-repository draft PR, create and
apply `zones-benchmark-deposit` to opt into the PR-label trigger with workflow
defaults. The authorization job requires both the labeler and PR author to have
repository write access and rejects fork PRs. The phase runner can also be
executed directly on the benchmark host while the workflow is under review.

Every run uploads its rendered specs, `preflight.json`, and bench JSON report.
Although the dispatch form exposes activity and withdrawal choices so their
wiring can be reviewed, the authorization/configuration job rejects them with a
funded-fixture error before using the bare-metal runner. Do not treat either
phase as workflow-runnable until coordinated funded-fixture creation and restore
support is added.
