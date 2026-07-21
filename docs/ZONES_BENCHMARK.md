# Zones benchmark transaction generation

This benchmark support prepares three independent `txgen-tempo` workloads for an
L1 deposit, ordinary Zone activity, and a Zone withdrawal. It also renders a
production-shaped L1 -> Zone -> L1 scenario that correlates each journey across
both chains and waits for its deposit and withdrawal to finish. The independent
phase runner remains available when only one transaction class should be
measured.

## Production-shaped benchmark environment

Do not use Anvil or `tempo-zone dev` for performance results. The checked-in
provisioner follows Tempo's e2e benchmark shape and creates the benchmark target
on the runner itself:

1. `tempo-xtask generate-localnet` creates a two-validator Tempo genesis with
   DKG material and an explicitly supplied private mnemonic.
2. `tempo-xtask install-reference-zone-factory` installs the canonical EIP-2935
   history contract plus constructor-equivalent reference ZoneFactory,
   Verifier, and ZoneMessenger state before either validator database is
   initialized. The provisioner verifies that both validators return the
   canonical hash for a recent L1 block before creating the Zone.
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

The default workflow dispatch uses Tempo's `bloat=1` preset: 1,000 MiB of
storage across all four TIP-20s, imported identically into both Tempo L1
databases. State bloat is never imported into the Zone genesis or datadir. On
the 32-logical-CPU benchmark runner, the default partition is:

| Process | CPUs | HTTP RPC | Metrics |
| --- | --- | --- | --- |
| Tempo validator A | `0-3,16-19` | `127.0.0.1:8545` | `127.0.0.1:9001` |
| Tempo validator B | `4-7,20-23` | `127.0.0.1:8645` | `127.0.0.1:9101` |
| Zone sequencer | `8-13,24-29` | public/query `127.0.0.1:8546`; private submission `127.0.0.1:8544` | `127.0.0.1:9201` |
| txgen and bench | `14-15,30-31` | n/a | n/a |

The two Tempo databases use separate restored Schelk volumes. The Zone datadir
shares validator A's volume but not its database. The provisioner accepts
`ZONES_BENCH_L1_A_CPUS`, `ZONES_BENCH_L1_B_CPUS`, and
`ZONES_BENCH_ZONE_CPUS` overrides when a runner has a different CPU topology.
That shared device is sufficient to exercise the complete topology, but it is
not production-like I/O isolation: Zone writes and validator A writes can
contend. A third benchmark device, passed through a future separate Zone state
root, is required before treating storage-sensitive results as production
comparable. The workflow records the runner CPU and block-device topology in
its artifacts so this constraint is visible with each result.

The pinned Tempo harness uses a `1,000,000,000,000` L1 gas limit; the provisioner
sets that value for both the block and general non-payment transaction gas
limits in genesis and for each validator's payload builder. Override
`ZONES_BENCH_L1_GAS_LIMIT` or `ZONES_BENCH_L1_GENERAL_GAS_LIMIT` only when
deliberately testing another Tempo setup. Keeping the general limit explicit is
important because bridge deposits are non-payment calls.

The temporary bloat dump is also placed on validator A's benchmark volume, not
the runner's root filesystem; `ZONES_BENCH_BLOAT_TMP_DIR` can override it.
Before generating a nonzero dump, the provisioner applies Tempo's free-space
rule: seven times the bloat size for each import plus a 51,200 MiB margin, with
one additional dump size on validator A. The default 1,000 MiB run therefore
requires 59,200 MiB free on A and 58,200 MiB on B, before additional Zone growth
on A.

Independent `bench send` phases use `txpool_status` for their drain check. The
selected trusted RPC must expose the `txpool` module, or the benchmark must
explicitly use a zero drain timeout. Every generated workload still waits for
its transaction receipt, but zero skips the final global pool-empty check and
weakens isolation between consecutive runs. The independent phase runner
validates `txpool_status` before sending when the drain timeout is non-zero. The
roundtrip scenario instead uses receipt-scoped and cross-chain event completion
boundaries.

This is production-shaped rather than production-equivalent. Zones is currently
documented as testnet-only, the optional multi-sequencer topology has a static
leader with no automatic promotion, and proof generation is not final. The
reference factory deploys full portal bytecode, while the proposed native
TIP-1091 lifecycle uses a protocol-managed implementation, so portal deployment
gas is not final-production exact.

The provisioned Zone uses the node's default state-root, trie, transaction
prewarming, and execution-cache settings. The pinned Reth revision can report
that its asynchronous state-root task disagreed with the block header; Reth then
recomputes the root synchronously before accepting or rejecting that block. The
benchmark records that fallback separately and only treats a final synchronous
state-root mismatch as a Zone failure.

### Roundtrip bootstrap and fixture boundaries

The provisioner stops at infrastructure readiness. The roundtrip runner then
performs a real, untimed bootstrap before starting measured journeys:

1. The control account at mnemonic index 0 approves the portal and deposits the
   configured bootstrap amount to the sequencer at index 4.
2. A bootstrap txgen scenario identifies the receipt-scoped `DepositMade` event
   and waits for the matching Zone `DepositProcessed` event and the L1
   `BatchSubmitted` commitment for that deposit number. The sequencer is
   therefore funded by portal-backed tokens, not an artificial Zone-genesis
   allocation.
3. The sequencer submits the untimed `ZoneOutbox.setTempoGasRate` transaction,
   making the withdrawal protocol fee nonzero.
4. Roundtrip preflight verifies the ready state and renders one user-signed
   outbox approval per benchmark account. The sequencer sponsors the Zone fees
   for those approvals, so benchmark users can start with zero Zone balance.
   The runner submits the independent expiring-nonce approvals concurrently and
   confirms every receipt before measurement begins.
5. `txgen-tempo auth-token-map` derives a short-lived authorization token for
   every benchmark sender. The measured scenario submits Zone transactions to
   the authenticated private RPC on port 8544 while chain, nonce, checkpoint,
   and log queries use the public endpoint on port 8546.

Each measured journey leases one pool account and executes:

1. a Tempo L1 deposit to that account with a unique random memo;
2. a wait for its exact `DepositMade` and `DepositProcessed` events;
3. an ordinary Zone TIP-20 transfer using an expiring nonce;
4. the exact eight-argument withdrawal request; and
5. a wait for the corresponding Tempo L1 `WithdrawalProcessed` event.

The preflight fixture assertions distinguish the supported starting points:

- `--fixture-state empty` requires no recorded or processed deposits, no portal
  escrow, and zero Zone balance for the benchmark pool. It is valid for the
  bootstrap or independent deposit checks.
- `--fixture-state ready` requires the control-to-sequencer bootstrap deposit to
  be processed and backed by portal escrow while every measured benchmark user
  still has zero Zone balance. It is used for the roundtrip check.
- `--fixture-state funded` is retained for independent activity, withdrawal, or
  combined phase runs against a separately prepared pool. It requires deposits
  to be fully processed, nonzero pool Zone balance, and sufficient portal
  backing.

Unbacked Zone-genesis balances are not a valid benchmark fixture.

### Provision and stop the topology manually

Build the Zone and pinned Tempo binaries in an optimized profile, then pass
their exact paths to the provisioner. To match the workflow's `bloat=1` preset
in a manual run, set the L1 bloat size to 1000 MiB. Direct script invocation
defaults to zero for a faster topology smoke test.

```bash
export TEMPO_ROOT="$HOME/projects/tempo"

cargo build --profile profiling -p tempo-zone -p tempo-xtask
cargo build --manifest-path "$TEMPO_ROOT/Cargo.toml" \
  --profile profiling --bin tempo
cargo build --manifest-path "$TEMPO_ROOT/Cargo.toml" \
  --profile profiling -p tempo-xtask

export ZONE_BIN="$PWD/target/profiling/tempo-zone"
export ZONES_XTASK_BIN="$PWD/target/profiling/tempo-xtask"
export TEMPO_BIN="$TEMPO_ROOT/target/profiling/tempo"
export TEMPO_XTASK_BIN="$TEMPO_ROOT/target/profiling/tempo-xtask"
export ZONES_BENCH_MNEMONIC='<private mnemonic for this isolated run>'
export ZONES_BENCH_ACCOUNT_START=16
export ZONES_BENCH_ACCOUNTS=100
export ZONES_BENCH_BLOAT_MIB=1000
export ZONES_BENCH_TOPOLOGY_DIR="$PWD/target/zones-benchmark/topology"
export ZONES_BENCH_STATE_A_ROOT='/reth-bench-a/zones-manual-<unique-run-id>'
export ZONES_BENCH_STATE_B_ROOT='/reth-bench-b/zones-manual-<unique-run-id>'
export ZONES_BENCH_ENV_FILE="$PWD/target/zones-benchmark/topology.env"
export ZONES_BENCH_PID_FILE="$PWD/target/zones-benchmark/topology.pids"

contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
```

`up` leaves all three node processes alive. It also configures nonzero portal
deposit and bounce-back fee rates before declaring the topology ready and
exports both `ZONE_RPC_URL` for public queries and `ZONE_PRIVATE_RPC_URL` for
authenticated submissions. The withdrawal rate cannot be configured on a fresh
Zone because that transaction itself needs Zone fee token. The roundtrip setup
first makes and observes the control-to-sequencer bootstrap deposit, then runs
this untimed action:

```bash
SEQUENCER_KEY='<private sequencer key>' \
  "$ZONES_XTASK_BIN" configure-benchmark-fees \
  --l1-rpc-url "$L1_RPC_URL" \
  --portal "$L1_PORTAL_ADDRESS" \
  --token "$ZONES_BENCH_TOKEN" \
  --zone-rpc-url "$ZONE_RPC_URL" \
  --tempo-gas-rate 1 \
  --zone-tx-gas-limit 2000000
```

Pass the sequencer key only through the environment. Do not place it in a
scenario, rendered artifact, command-line option, or workflow input.

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
- checks token balances and allowances for every benchmark account plus the
  distinct control and sequencer accounts;
- rejects phase amounts that do not cover protocol and transaction fee budgets;
- checks that a roundtrip deposit can cover its Zone activity, withdrawal,
  protocol fees, and transaction fee caps;
- calculates the minimum sequencer bootstrap amount needed for fee
  configuration and sponsored user approvals;
- renders the independent specs, bootstrap and roundtrip workloads, both
  scenario documents, and the minimal event ABIs;
- injects required per-user portal approvals and sequencer-sponsored outbox
  approvals as untimed expiring-nonce txgen setup transactions; and
- writes a non-secret `preflight.json` report.

It does not create a Zone, fund an account, submit an approval or workload
transaction, wait for cross-chain state, or run txgen. The roundtrip runner
invokes it before and after the separate bootstrap scenario; txgen performs the
submissions and event waits.

## Prerequisites

Install `txgen-tempo` and `bench` from the exact combined txgen revision used by
this workflow (Rust 1.93 or newer is required):

```bash
export TXGEN_REV='f1fe55ea308b7f44b81bbc2322992a71d4522a03'
cargo install --git https://github.com/tempoxyz/txgen \
  --rev "$TXGEN_REV" --locked txgen-tempo bench-cli
```

That commit is currently reachable through the public
`tempoxyz/txgen:dan/zones-716-txgen` branch. The branch must remain reachable for
fresh `cargo install --rev` operations until the required txgen changes merge;
afterward this workflow should be repinned to their reachable merge commit.

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

There is no fallback mnemonic, public test mnemonic, public write endpoint, or
mainnet write endpoint. The provisioner funds the derived pool only inside the
isolated Tempo genesis. The roundtrip uses real deposits for every Zone balance;
preflight fails instead of silently funding or bridging an account.

## Run preflight

On a newly provisioned topology, first validate the empty state and render the
bootstrap assets:

```bash
journeys_per_account=$((
  (ZONES_BENCH_COUNT + ZONES_BENCH_ACCOUNTS - 1) / ZONES_BENCH_ACCOUNTS
))

cargo run -p tempo-xtask -- benchmark-preflight \
  --l1-rpc-url "$L1_RPC_URL" \
  --zone-rpc-url "$ZONE_RPC_URL" \
  --token "$ZONES_BENCH_TOKEN" \
  --account-start "$ZONES_BENCH_ACCOUNT_START" \
  --accounts "$ZONES_BENCH_ACCOUNTS" \
  --deposit-amount "$ZONES_BENCH_DEPOSIT_AMOUNT" \
  --activity-amount "$ZONES_BENCH_ACTIVITY_AMOUNT" \
  --withdrawal-amount "$ZONES_BENCH_WITHDRAWAL_AMOUNT" \
  --bootstrap-deposit-amount "$ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT" \
  --transactions-per-account "$journeys_per_account" \
  --check-phase bootstrap \
  --fixture-state empty \
  --output target/zones-benchmark
```

Pass `--zone-dir generated/<zone>` when generated Zone metadata should be used
to resolve and cross-check the deployed addresses. After the bootstrap deposit
has reached the Zone and the sequencer has configured the nonzero outbox rate,
rerun the same command with `--check-phase roundtrip --fixture-state ready`.
A separately prepared real deposit fixture uses `--check-phase activity`,
`withdrawal`, or `all` with `--fixture-state funded`.

Preflight always queries and reports both networks. The selected check controls
which balance capacity and approval setup actions are enforced. Its output
contains `deposit.yml`, `zone-activity.yml`, `withdrawal.yml`,
`bootstrap-deposit.yml`, `zone-roundtrip.yml`, `bootstrap-scenario.yml`,
`roundtrip-scenario.yml`, `preflight.json`, and their minimal ABI artifacts.

Set `--transactions-per-account` to a conservative upper bound for how many
measured journeys or transactions any one account may send. Preflight uses that
capacity when checking token balances, transaction-fee headroom, and allowances;
its default is one. The roundtrip scenario uses leased accounts and requires
`--max-in-flight` not to exceed the account pool; budget the maximum number of
journeys that can reuse any one account. Independent phase specs select senders
randomly, so the phase runner conservatively budgets the full transaction count
for every account. For expiring Zone activity and approval setup, preflight also
budgets txgen's monotonic fee-cap uniqueness bump through the configured
capacity.

When a per-user approval is needed, the rendered workload includes it as a
txgen setup transaction. Scenario initialization or `bench send` submits setup
transactions and confirms them before measurement starts. Roundtrip outbox
approvals are signed by each benchmark user and fee-sponsored by the
bootstrapped sequencer. These approvals use 25-second expiring nonces and must
be generated immediately before submission rather than saved for later replay.
The single control-account approval in the bootstrap remains a regular-nonce
transaction.

## Run the full roundtrip

With the topology environment loaded, the supported entry point performs the
entire bootstrap, preflight, authorization, and measured sequence:

```bash
export ZONES_BENCH_SEED='<unique unsigned integer>'
export ZONES_BENCH_ACCOUNTS=100
export ZONES_BENCH_COUNT=100
export ZONES_BENCH_TPS=10
export ZONES_BENCH_MAX_CONCURRENT=100
export ZONES_BENCH_DEPOSIT_AMOUNT=2000000
export ZONES_BENCH_ACTIVITY_AMOUNT=1
export ZONES_BENCH_WITHDRAWAL_AMOUNT=1000000
export ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT=10000000

contrib/bench/run-roundtrip.sh
```

The runner calculates `ceil(count / accounts)` for preflight's per-account
journey capacity, refuses more concurrent journeys than leased accounts, keeps
the mnemonic and sequencer key out of argv, validates both scenario reports,
and deletes its authorization map on exit. It renders approval-only streams,
submits each account's expiring-nonce approval concurrently, logs receipt-wait
heartbeats and completion counts, then reruns preflight with setup disabled to
verify every allowance. While the measured scenario runs, it prints the number
of successful events observed at every leg every 10 seconds by default. On this
fresh topology the terminal L1 `WithdrawalProcessed` count is the externally
visible completion count; txgen's authoritative journey report is still
checked at the end.
The same monitor fails fast on a dead Zone process, invalid payload, final state
root mismatch, or panic. A recoverable asynchronous state-root mismatch is
printed and recorded in `zone-state-root-fallbacks.log`, then left to Reth's
synchronous verification path.

The equivalent stages are described below for diagnosis or controlled manual
execution.

After the empty/bootstrap preflight above, run the one-instance bootstrap
scenario. It is setup traffic and is not part of the measured roundtrip report:

```bash
txgen-tempo scenario run \
  --scenario target/zones-benchmark/bootstrap-scenario.yml \
  --count 1 \
  --max-in-flight 1 \
  --seed "$ZONES_BENCH_SEED" \
  --failure-policy fail-fast \
  --report target/zones-benchmark/bootstrap-report.json
```

After that command has observed `DepositProcessed`, configure the nonzero
withdrawal rate with the sequencer key through the environment, then rerun
preflight with `--check-phase roundtrip --fixture-state ready`. The second
preflight verifies that measured users still start with zero Zone balance, that
the sequencer can sponsor every required approval, and that each measured
deposit can pay for the activity and withdrawal that follow it.

Generate the private-RPC sender map from the rendered user pool. Keep the map in
a mode-0700 temporary directory outside the uploaded artifact tree;
`auth-token-map` creates and atomically refreshes the file with mode 0600. Watch
mode is needed when a run can outlive the initial token TTL.

```bash
auth_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/zones-benchmark-auth.XXXXXX")"
chmod 700 "$auth_dir"
export ZONES_BENCH_ZONE_AUTH_MAP="$auth_dir/zone-auth.json"

txgen-tempo auth-token-map \
  --spec target/zones-benchmark/zone-roundtrip.yml \
  --pool users \
  --zone-id "$ZONES_BENCH_EXPECTED_ZONE_ID" \
  --chain-id "$ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID" \
  --ttl-secs 600 \
  --refresh-before-secs 30 \
  --watch \
  --output "$ZONES_BENCH_ZONE_AUTH_MAP" &
auth_map_pid=$!
```

Do not print, upload, or include the authorization map in the workflow summary.
The map contains bearer-equivalent sender credentials even though it contains
neither the mnemonic nor raw private keys.

Once the initial map exists, start the measured scenario:

```bash
txgen-tempo scenario run \
  --scenario target/zones-benchmark/roundtrip-scenario.yml \
  --count "$ZONES_BENCH_COUNT" \
  --starts-per-second "$ZONES_BENCH_TPS" \
  --max-in-flight "$ZONES_BENCH_MAX_CONCURRENT" \
  --max-rpc-in-flight "$ZONES_BENCH_MAX_CONCURRENT" \
  --seed "$ZONES_BENCH_SEED" \
  --failure-policy continue \
  --report target/zones-benchmark/roundtrip-report.json

kill "$auth_map_pid"
wait "$auth_map_pid" || true
rm -f -- "$ZONES_BENCH_ZONE_AUTH_MAP"
rmdir -- "$auth_dir"
```

For a scenario, `--starts-per-second` is the number of complete journeys
started per second, not raw transaction TPS. `--tx-rate` can independently cap
submissions on each chain when that is useful. Each journey holds its leased
account until its terminal L1 withdrawal event or failure.

Render the same results page published on the GitHub workflow overview from a
completed local run with:

```bash
cargo run -p tempo-xtask -- benchmark-results \
  --report target/zones-benchmark/roundtrip-report.json \
  --scenario target/zones-benchmark/roundtrip-scenario.yml \
  --output target/zones-benchmark/summary.md
```

The renderer validates that the report and scenario steps still match before
using each step's configured chain to calculate aggregate, per-chain, and
per-submit user throughput. It does not need the benchmark mnemonic or RPC
access.

## Run one phase

`contrib/bench/run-phase.sh` combines preflight, spec selection, generation, and
submission for exactly one phase. It does not run a phase before or after the
selected one.

```bash
export ZONES_BENCH_SEED='<unique unsigned integer>'
export ZONES_BENCH_ACCOUNTS=100
export ZONES_BENCH_COUNT=100
export ZONES_BENCH_TPS=100
export ZONES_BENCH_MAX_CONCURRENT=100

contrib/bench/run-phase.sh deposit
# or: contrib/bench/run-phase.sh activity
# or: contrib/bench/run-phase.sh withdrawal
```

`deposit` is valid against the empty topology. `activity` and `withdrawal` are
independent entry points for a verified, portal-backed funded fixture; they are
not an implicit phase chain. Use the roundtrip scenario above when the benchmark
should perform and time the complete L1 -> Zone -> L1 journey itself.

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
file for later replay. For production-shaped private submission, generate a
sender map from this spec's `users` pool as shown in the roundtrip section.

```bash
txgen-tempo generate \
  --spec target/zones-benchmark/zone-activity.yml \
  --count "$ZONES_BENCH_COUNT" \
  --seed "$ZONES_BENCH_SEED" \
  --rpc "$ZONE_RPC_URL" \
| bench send \
  --rpc-url "$ZONE_PRIVATE_RPC_URL" \
  --query-rpc-url "$ZONE_RPC_URL" \
  --sender-header-name X-Authorization-Token \
  --sender-header-map "$ZONES_BENCH_ZONE_AUTH_MAP" \
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
  --rpc-url "$ZONE_PRIVATE_RPC_URL" \
  --query-rpc-url "$ZONE_RPC_URL" \
  --sender-header-name X-Authorization-Token \
  --sender-header-map "$ZONES_BENCH_ZONE_AUTH_MAP" \
  --tps "$ZONES_BENCH_TPS" \
  --max-concurrent "$ZONES_BENCH_MAX_CONCURRENT"
```

## GitHub workflow

`.github/workflows/zones-benchmark.yml` runs a self-provisioned roundtrip or an
independent deposit workload on the private `bare-metal-dual-schelk` runner
class used by Tempo's e2e benchmark. It does not use a GitHub Actions
environment, a pre-existing Zone, or an externally configured write RPC. Each
job:

1. restores the two isolated Schelk volumes and assigns unique state roots;
2. checks out the exact Tempo revision and txgen commit
   `f1fe55ea308b7f44b81bbc2322992a71d4522a03`, then builds Tempo and Zone
   binaries with the e2e benchmark's `profiling` profile and
   `-C target-cpu=native`;
3. generates a fresh private 24-word mnemonic for the run;
4. applies the pinned Tempo benchmark host tuning and invokes its cleanup hook
   during teardown;
5. generates the two-validator L1, optionally imports the selected four-token
   state bloat into both L1 databases, creates an unbloated real Zone, and
   starts all nodes;
6. for `roundtrip`, performs the real control-to-sequencer bootstrap deposit and
   waits for its `DepositProcessed` and L1 `BatchSubmitted` events, configures
   the nonzero outbox fee, creates the short-lived sender-auth map, confirms
   sponsored user approvals, and runs the measured
   deposit -> wait -> activity -> withdrawal -> wait scenario;
7. for `deposit`, runs the independent preflight/generate/bench pipeline; and
8. renders the JSON report into a Markdown results page on the workflow
   overview, then uploads that page with the rendered non-secret assets,
   host/storage metadata, JSON reports, and node logs before stopping the nodes
   and restoring the benchmark volumes.

Repeat runs use the pinned Tempo benchmark's commit-and-feature-keyed MinIO
cache for the `tempo` binary. Cache misses build from source and, when the
runner's MinIO alias is available, populate it through `tempo.nu`. Separate
Cargo target directories under the runner tool cache retain unchanged Zones and
`tempo-xtask` artifacts across clean checkouts. These caches contain build
outputs only; every run still generates fresh L1 databases, L1 state bloat,
identities, contracts, and Zone state.

Tempo and txgen are fetched from public repositories at exact commits, so no
dependency-access secret is required. No mnemonic, portal, chain ID, token,
target ID, or RPC URL needs to be configured externally. The workflow never
selects a public test mnemonic or a public/mainnet write endpoint. Its private
sender-auth map is created outside the artifact tree with mode 0600 and deleted
on exit.

After the workflow exists on the default branch, dispatch it from the Actions
UI or CLI and select the branch/ref to test:

```bash
gh workflow run zones-benchmark.yml \
  --ref '<branch-or-tag>' \
  -f phase=roundtrip \
  -f accounts=100 \
  -f count=100 \
  -f tps=10 \
  -f state-bloat-gib=1
```

GitHub does not expose a newly introduced `workflow_dispatch` until the workflow
file exists on the default branch. Before merge, opening, reopening, or pushing
a commit to a same-repository PR whose cumulative diff touches the workflow,
`contrib/bench`, `xtask`, or this document runs `roundtrip` with the workflow
defaults. The authorization job requires both the triggering actor and PR
author to have repository write access and rejects fork PRs. A newer commit
cancels an obsolete run for the same PR, while benchmark jobs from different
PRs and manual dispatches serialize on the shared Schelk host resources. The
scripts can also be executed directly on the benchmark host while the workflow
is under review.

This is a public repository, and a `pull_request` run evaluates the workflow
definition from the PR. The GitHub repository or organization policy must
require approval for outside-collaborator workflows before they can reach a
self-hosted runner. The same-repository and write-permission checks above are
defense in depth; they do not replace that GitHub-side policy.

Activity and withdrawal are intentionally not dispatch choices because this
self-provisioning workflow starts from a fresh pool with no pre-existing Zone
balance. Run their independent assets manually against a fixture that passes
`--fixture-state funded`, or select `roundtrip` to create balances within each
measured journey.

### Current scenario reporting limits

The txgen scenario engine accepts one submission URL per named chain. This
workflow submits measured L1 user transactions through validator A and uses
validator B for aggregate queries; unlike the independent deposit pipeline, it
does not spread L1 submissions across both validators.

Scenario mode writes its journey and per-step latency JSON report, but it does
not scrape the node metric endpoints and has no ClickHouse benchmark reporter.
The workflow combines that report with the rendered scenario to publish a
scenario-native results page. It reports completed journeys per second,
aggregate and per-chain submitted user TPS, whole-journey latency, and latency
for every measured submit and wait step. The generated Markdown is also
included in the run artifact.

These rates cover the complete measured window, including ramp-up and drain;
they are not a saturation or single-chain capacity claim. Aggregate user TPS
sums successful submit steps across distinct chains. Submit-step latency ends
when the RPC accepts the transaction, while receipt and log waits report the
subsequent execution and cross-chain progress. Untimed bootstrap and approval
setup is excluded. The page does not claim the node-metric/ClickHouse reporting
available to Tempo's existing single-chain benchmark harness.

The pinned txgen scenario runtime currently serializes expiring-nonce activity
submissions through one internal fee-uniqueness scheduling lane until each RPC
accepts the transaction. Its capacity depends on RPC acceptance latency and it
can cap measured Zone activity at any configured rate; it must be fixed upstream
before interpreting that path as unconstrained throughput. Reverted withdrawals
and rejected bridge outcomes also surface as step timeouts rather than immediate
terminal classifications.

The workflow pins disjoint CPU sets but runs two validators, one Zone, and the
sender on one 32-logical-CPU host without the two-validator harness's 60 GiB
per-process memory scopes. Combined with the shared Zone/validator-A device,
this means results describe this explicit single-host topology rather than
separate production hosts.

Tempo's pinned `restore-system-tuning` cleanup hook restarts `cron`, but it does
not restore the prior sysctls, CPU governor/turbo settings, swap, transparent
huge-page settings, or `unattended-upgrades` service state. The dedicated runner
therefore remains benchmark-tuned after this workflow, just as it does after
Tempo's e2e benchmark.

Finally, the pinned txgen commit is on an unmerged branch. The branch must remain
reachable until those changes merge and this workflow is repinned to a
reachable merge commit.
