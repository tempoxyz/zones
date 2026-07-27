# Zones benchmark transaction generation

This benchmark support prepares three independent `txgen-tempo` workloads for an
L1 deposit, ordinary Zone activity, and a Zone withdrawal. It also renders a
production-shaped L1 -> Zone -> L1 scenario that correlates each journey across
both chains and waits for its deposit and withdrawal to finish. The independent
phase runner remains available when only one transaction class should be
measured. The neobank profile additionally exposes focused encrypted-deposit
and private-withdrawal scenarios that provision their own valid starting state
before measuring one cross-chain direction.

The generic and neobank assets are maintained together on the repository's
`main` line: topology provisioning, preflight, scenario execution, correlation,
reporting, and workflow dispatch are one benchmark implementation. Running the
neobank profile does not depend on a stacked benchmark branch or a separately
provisioned topology.

## Production-shaped benchmark environment

Do not use Anvil or `tempo-zone dev` for performance results. The checked-in
provisioner follows Tempo's e2e benchmark shape and creates the benchmark target
on the runner itself:

1. `tempo-xtask generate-localnet` creates a two-validator Tempo genesis with
   DKG material and an explicitly supplied private mnemonic file.
2. `tempo-xtask install-reference-zone-factory` installs the canonical EIP-2935
   history contract, native ZoneFactory marker and packed configuration, plus
   the shared Portal, Verifier, and ZoneMessenger runtimes before either
   validator database is initialized. The provisioner verifies that both
   validators return the canonical hash for a recent L1 block before creating
   the Zone.
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
databases. State bloat is never imported into the Zone genesis or datadir.

The two L1 databases are a persistent, runner-local baseline. On a cache miss,
the workflow generates and patches Tempo genesis, initializes both databases,
imports the selected bloat, and promotes the completed Schelk scratch volumes
as the new immutable virgin snapshots. On a cache hit, Schelk restores private
writable copies of those snapshots and skips genesis and bloat generation. The
baseline is accepted only when both volumes contain the same generation and a
manifest matching the exact Tempo revision, mnemonic-derived public identities,
patched genesis and reference-factory inputs, chain and gas limits, and bloat
size, token layout, balance, and funded-account capacity. A missing or mismatched
side rebuilds the pair; `force-bloat=true` deliberately rebuilds it even when the
manifest matches. The shared generation identifier also makes a partially
completed two-volume promotion a cache miss on the next run.

The cached boundary is before validator startup, Zone creation, fee
configuration, bootstrap deposits, and other fixtures. Every benchmark run
therefore starts the validators on a private restored copy and creates a fresh
Zone, portal, and runtime state on top. A run can mutate only its restored copy,
never the cached virgin baseline. The snapshots stay on the benchmark runner;
they are not uploaded to GitHub, included in workflow artifacts, or shared
through the binary/build cache.

The stable account identity needed by the cached funding state is held in a
mode-0600 private mnemonic file on the runner. The file is reused with the
baseline and is never passed as a command-line argument, copied into the
topology output, or uploaded as an artifact. Rotating or replacing it changes
the public-identity fingerprint in the manifest and forces the L1 pair to be
rebuilt. There is no public test-mnemonic fallback.

On the 32-logical-CPU benchmark runner, the default partition is:

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

The benchmark topology uses a `30,000,000` L1 gas limit to match the production
limit. The provisioner sets that value for both the block and general
non-payment transaction gas limits in genesis and for each validator's payload
builder. Override
`ZONES_BENCH_L1_GAS_LIMIT` or `ZONES_BENCH_L1_GENERAL_GAS_LIMIT` only when
deliberately testing another Tempo setup. Keeping the general limit explicit is
important because bridge deposits are non-payment calls.

On a baseline rebuild, the temporary bloat dump is placed on validator A's
benchmark volume, not the runner's root filesystem;
`ZONES_BENCH_BLOAT_TMP_DIR` can override it. Before generating a nonzero dump,
the provisioner applies Tempo's free-space rule: seven times the bloat size for
each import plus a 51,200 MiB margin, with one additional dump size on validator
A. The default 1,000 MiB rebuild therefore requires 59,200 MiB free on A and
58,200 MiB on B, before additional Zone growth on A. A matching cache hit does
not regenerate the dump or perform either import.

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
local genesis installs the native TIP-1091 marker and the same shared runtime
boundaries used by the pinned Tempo implementation.

The pinned Reth revision predates the retained-branch pruning fix in
[reth#26376](https://github.com/paradigmxyz/reth/pull/26376). The provisioned
Zone passes `--engine.disable-sparse-trie-cache-pruning` to avoid incorrect
roots from a reused, pruned sparse trie. The asynchronous state-root task,
reusable trie, transaction prewarming, and execution cache remain enabled.
Remove the override after Zones updates to a Reth revision containing that fix.

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
export ZONES_ROOT="$PWD"
export TEMPO_ROOT="$HOME/projects/tempo"
export TEMPO_REV='2163eb53a6419b53f4388bfde9177ea96dffb570'

# Use a clean, dedicated Tempo checkout at TEMPO_REV for this benchmark.
test "$(git -C "$TEMPO_ROOT" rev-parse HEAD)" = "$TEMPO_REV"
git -C "$TEMPO_ROOT" apply \
  "$ZONES_ROOT/contrib/bench/patches/tempo-xtask-mnemonic-file.patch"

cargo build --profile profiling -p tempo-zone -p tempo-xtask
cargo build --manifest-path "$TEMPO_ROOT/Cargo.toml" \
  --profile profiling --bin tempo
cargo build --manifest-path "$TEMPO_ROOT/Cargo.toml" \
  --profile profiling -p tempo-xtask

export ZONE_BIN="$ZONES_ROOT/target/profiling/tempo-zone"
export ZONES_XTASK_BIN="$ZONES_ROOT/target/profiling/tempo-xtask"
export TEMPO_BIN="$TEMPO_ROOT/target/profiling/tempo"
export TEMPO_XTASK_BIN="$TEMPO_ROOT/target/profiling/tempo-xtask"
export ZONES_BENCH_MNEMONIC_FILE='/secure/runner-local/zones-benchmark-mnemonic'
export ZONES_BENCH_ACCOUNT_START=16
export ZONES_BENCH_ACCOUNTS=200
export ZONES_BENCH_BLOAT_MIB=1000
export ZONES_BENCH_TOPOLOGY_DIR="$PWD/target/zones-benchmark/topology"
export ZONES_BENCH_STATE_A_ROOT='/reth-bench-a/zones-l1-1000mb'
export ZONES_BENCH_STATE_B_ROOT='/reth-bench-b/zones-l1-1000mb'
export ZONES_BENCH_ZONE_STATE_ROOT='/reth-bench-a/zones-runtime-<unique-run-id>'
export ZONES_BENCH_L1_CACHE_STATUS_FILE="$PWD/target/zones-benchmark/l1-cache.env"
export ZONES_BENCH_ENV_FILE="$PWD/target/zones-benchmark/topology.env"
export ZONES_BENCH_PID_FILE="$PWD/target/zones-benchmark/topology.pids"

nu "$TEMPO_ROOT/bench-schelk.nu" restore \
  /var/lib/schelk/a.json /reth-bench-a
nu "$TEMPO_ROOT/bench-schelk.nu" restore \
  /var/lib/schelk/b.json /reth-bench-b
nu "$TEMPO_ROOT/bench-schelk.nu" mark-dirty /var/lib/schelk/a.json
nu "$TEMPO_ROOT/bench-schelk.nu" mark-dirty /var/lib/schelk/b.json

contrib/bench/provision-topology.sh prepare-l1
source "$ZONES_BENCH_L1_CACHE_STATUS_FILE"

if [[ "$ZONES_BENCH_L1_CACHE_REBUILT" == 1 ]]; then
  nu "$TEMPO_ROOT/bench-schelk.nu" promote /var/lib/schelk/a.json
  nu "$TEMPO_ROOT/bench-schelk.nu" promote /var/lib/schelk/b.json
  nu "$TEMPO_ROOT/bench-schelk.nu" restore \
    /var/lib/schelk/a.json /reth-bench-a
  nu "$TEMPO_ROOT/bench-schelk.nu" restore \
    /var/lib/schelk/b.json /reth-bench-b
  nu "$TEMPO_ROOT/bench-schelk.nu" mark-dirty /var/lib/schelk/a.json
  nu "$TEMPO_ROOT/bench-schelk.nu" mark-dirty /var/lib/schelk/b.json
  contrib/bench/provision-topology.sh verify-l1
fi

contrib/bench/provision-topology.sh up
source "$ZONES_BENCH_ENV_FILE"
```

The mnemonic file must contain only the private BIP-39 phrase and must be
readable only by the benchmark user (`chmod 600`). Its path, not its contents,
is passed to the provisioner. `prepare-l1` validates both baseline manifests and
writes `ZONES_BENCH_L1_CACHE_REBUILT=0` or `1`, plus non-secret cache key and
generation metadata, to `ZONES_BENCH_L1_CACHE_STATUS_FILE`. Set
`ZONES_BENCH_FORCE_BLOAT=1` before `prepare-l1` to rebuild a matching baseline;
the default is `0`.

Promotion is deliberately outside the provisioner because it is a Schelk
snapshot operation owned by the workflow. When `prepare-l1` reports a rebuild,
promote both sides before starting any node, immediately restore new private
copies, and run `verify-l1`. Do not call `up` between promotion and that restore:
the post-promotion restore is what isolates runtime writes from the virgin
baseline. On a cache hit, the initial workflow restore is already the private
copy and no promotion is needed. `prepare-l1` and `verify-l1` never create a
Zone; `up` always creates one under the unique
`ZONES_BENCH_ZONE_STATE_ROOT`.

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

Always stop the recorded processes with the matching PID file, then recover the
two Schelk scratch volumes:

```bash
contrib/bench/provision-topology.sh cleanup "$ZONES_BENCH_PID_FILE"
nu "$TEMPO_ROOT/bench-schelk.nu" cleanup /var/lib/schelk/a.json
nu "$TEMPO_ROOT/bench-schelk.nu" cleanup /var/lib/schelk/b.json
```

The stable L1 paths are expected to exist across restored snapshots. The
provisioner rejects a reused topology directory or Zone runtime path; choose a
new value for each `up` run. The pinned Tempo xtask is patched at build time to
accept `--mnemonic-file` for localnet and state-bloat generation, keeping the
phrase out of process arguments. That patch is part of the L1 cache key.

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
export TXGEN_REV='072877b673f60b5f559f17da098296f1841b6732'
cargo install --git https://github.com/tempoxyz/txgen \
  --rev "$TXGEN_REV" --locked txgen-tempo bench-cli
```

That pinned `tempoxyz/txgen:main` revision includes DAG scenario execution,
subscription-backed observation with canonical polling fallback, causal report
schema v2, sampled lifecycle traces, receipt-gas publication, and the
encrypted-deposit and callback encoding support.

The paired-manifest and safe-root checks are standalone and do not start nodes
or build contracts:

```bash
contrib/bench/tests/l1-snapshot.sh
```

The render/generation compatibility test can be run with:

```bash
TXGEN_TEMPO_BIN="$(command -v txgen-tempo)" \
  cargo test -p tempo-xtask \
  txgen_generates_representative_local_transactions_when_installed
```

For manual phase generation, point at the same private mnemonic file used to
build the L1 baseline and load the provisioner's non-secret RPC/address output:

```bash
export ZONES_BENCH_MNEMONIC_FILE='/secure/runner-local/zones-benchmark-mnemonic'
source target/zones-benchmark/topology.env
```

There is no fallback mnemonic, public test mnemonic, public write endpoint, or
mainnet write endpoint. Preflight and the runners read the phrase from that file
without printing or serializing it. The provisioner funds the derived pool only
inside the isolated Tempo genesis. The roundtrip uses real deposits for every
Zone balance; preflight fails instead of silently funding or bridging an
account.

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
  --recipient-mode "${ZONES_BENCH_RECIPIENT_MODE:-existing}" \
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
`bootstrap-deposit.yml`, `zone-roundtrip.yml`, `scenario-fragments.yml`,
`bootstrap-scenario.yml`, `roundtrip-scenario.yml`, `preflight.json`, and their
minimal ABI artifacts. Both scenario roots reuse the fragment library's
deposit-to-Zone submission and event-correlation steps.

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
export ZONES_BENCH_TPS=20
export ZONES_BENCH_MAX_CONCURRENT=12
export ZONES_BENCH_DEPOSIT_AMOUNT=2000000
export ZONES_BENCH_ACTIVITY_AMOUNT=1
export ZONES_BENCH_WITHDRAWAL_AMOUNT=1000000
export ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT=10000000
export ZONES_BENCH_RECIPIENT_MODE=existing

contrib/bench/run-roundtrip.sh
```

The runner calculates `ceil(count / accounts)` for preflight's per-account
journey capacity, refuses more concurrent journeys than leased accounts, keeps
the mnemonic and sequencer key out of argv, validates both scenario reports,
and deletes its authorization map on exit. It runs the composed scenario so the
txgen report retains fragment provenance, while emitting a flattened
`roundtrip-scenario.rendered.yml` for the Zones results renderer. It renders
approval-only streams, submits each account's expiring-nonce approval concurrently, logs receipt-wait
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

`ZONES_BENCH_RECIPIENT_MODE` controls destination-state reuse without changing
the sender pool:

- `existing` selects recipients from the funded benchmark account pool, so
  repeated transfers and withdrawals exercise already-existing destination
  accounts.
- `random` uses fresh, unfunded destinations. Independent activity and
  withdrawal streams derive seeded random addresses, while the roundtrip
  scenario leases a distinct deterministic destination for each journey.

The roundtrip threads the selected recipient through its activity transfer,
withdrawal request, and receipt-scoped event filters, so fresh destinations do
not weaken exact cross-chain correlation. The random destinations need no
private keys because the benchmark never spends from them.

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
txgen-tempo scenario render \
  --scenario target/zones-benchmark/roundtrip-scenario.yml \
  --output target/zones-benchmark/roundtrip-scenario.rendered.yml

txgen-tempo scenario run \
  --scenario target/zones-benchmark/roundtrip-scenario.yml \
  --count "$ZONES_BENCH_COUNT" \
  --starts-per-second "$ZONES_BENCH_TPS" \
  --max-in-flight "$ZONES_BENCH_MAX_CONCURRENT" \
  --max-rpc-in-flight "$ZONES_BENCH_MAX_CONCURRENT" \
  --seed "$ZONES_BENCH_SEED" \
  --failure-policy continue \
  --sample-instances 10 \
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

Run the original composed file to retain fragment provenance in txgen's report.
The flattened file is only for consumers such as `benchmark-results` that
expect concrete inline steps. `scenario render` expands environment-backed URLs
and paths; do not upload its output when those values contain credentials.

Render the same results page published on the GitHub workflow overview from a
completed local run with:

```bash
cargo run -p tempo-xtask -- benchmark-results \
  --report target/zones-benchmark/report-roundtrip.json \
  --scenario target/zones-benchmark/roundtrip/roundtrip-scenario.rendered.yml \
  --output target/zones-benchmark/summary.md
```

The renderer validates that the report and scenario steps still match before
using each step's configured chain to calculate aggregate, per-chain, and
per-submit user throughput. It does not need the benchmark mnemonic or RPC
access. For an independent phase report, omit `--scenario`; the page then shows
attempted and RPC-accepted transaction TPS plus RPC response latency when it
was collected. Both report types include receipt-derived gas usage, effective
gas price, and paid-fee distributions for each labeled transaction input.

## Run one phase

`contrib/bench/run-phase.sh` combines preflight, spec selection, generation, and
submission for exactly one phase. It does not run a phase before or after the
selected one.

```bash
export ZONES_BENCH_SEED='<unique unsigned integer>'
export ZONES_BENCH_ACCOUNTS=100
export ZONES_BENCH_COUNT=100
export ZONES_BENCH_TPS=100
export ZONES_BENCH_MAX_CONCURRENT=12
export ZONES_BENCH_RECIPIENT_MODE=existing

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
file for later replay. With `recipient-mode=existing`, the transfer target is a
reused benchmark account; with `recipient-mode=random`, it is a fresh,
seed-derived address. For production-shaped private submission, generate a
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
recipient follows `recipient-mode`: it is selected from the funded benchmark
pool in `existing` mode and is a fresh, seed-derived unfunded address in
`random` mode. The Zone fallback remains the selected sender's controlled
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

`.github/workflows/zones-benchmark.yml` runs the self-provisioned generic
roundtrip, neobank scenarios, or independent deposit workload on the private
`bare-metal-dual-schelk` runner class used by Tempo's e2e benchmark. It does not
use a GitHub Actions environment, a pre-existing Zone, or an externally
configured write RPC. Each job:

1. loads the stable private mnemonic from a mode-0600 file under the runner tool
   cache, generating it once when that runner has no identity file yet, without
   putting the phrase in workflow arguments or artifacts;
2. restores private writable copies of the two isolated Schelk virgin volumes;
3. checks out the exact Tempo revision and txgen commit
   `072877b673f60b5f559f17da098296f1841b6732`, then builds Tempo and Zone
   binaries with the e2e benchmark's `profiling` profile and
   `-C target-cpu=native`;
4. applies the pinned Tempo benchmark host tuning and invokes its cleanup hook
   during teardown;
5. validates the paired L1 cache manifest and either reuses the restored
   baseline or generates genesis and bloat once, promotes both volumes, restores
   private copies again, and verifies the promoted pair;
6. starts both validators from that private copy, creates a fresh unbloated real
   Zone in a unique runtime path, and starts its sequencer;
7. for `roundtrip`, performs the real control-to-sequencer bootstrap deposit and
   waits for its `DepositProcessed` and L1 `BatchSubmitted` events, configures
   the nonzero outbox fee, creates the short-lived sender-auth map, confirms
   sponsored user approvals, and runs the measured
   deposit -> wait -> activity -> withdrawal -> wait scenario;
8. for `neobank-e2e`, deploys the checked-in Earn boundary fixtures, configures
   the selected swap mechanism and closed-loop token and recipient policy,
   applies the file-backed benchmark account map after the 30M-bounded Zone
   creation transaction, prepares approvals and private RPC authorization, and
   runs the selected measured scenario. The `neobank-private-withdrawal`
   selection first creates and verifies DLUSD-backed private EarnShare
   positions outside measurement;
9. for `deposit`, runs the independent preflight/generate/bench pipeline; and
10. renders the JSON report into a Markdown results page on the workflow
   overview, then uploads that page with the rendered benchmark assets,
   host/storage metadata, JSON reports, and node logs before stopping the nodes
   and restoring the benchmark volumes.

The focused neobank dispatch choices use these boundaries and reporting
routes:

| Workflow phase | Untimed setup | Measured completion | Results route |
| --- | --- | --- | --- |
| `neobank-encrypted-deposit` | Fixture deployment and portal approval | Exact Zone `EncryptedDepositProcessed` correlated to the receipt-scoped L1 encrypted deposit | `/scenarios/neobank-encrypted-deposit` |
| `neobank-private-withdrawal` | DLUSD-backed private EarnShare positions and DLUSD/EarnShare outbox approvals | Receipt-scoped EarnShare withdrawal and Earn redemption, followed by the exact encrypted DLUSD `DepositProcessed` return | `/scenarios/neobank-private-withdrawal` |

### Dispatch configuration

The workflow exposes the load shape and topology controls needed to run the
same benchmark definition as a smoke test, a sustained load test, or a
mechanism comparison:

| Controls | Inputs and defaults |
| --- | --- |
| Workload | `phase=neobank-full-journey`, `accounts=100`, `count=100`, `tps=20`, `max-concurrent=12`, and an optional `seed` derived from the workflow run when empty |
| Transaction amounts | `deposit-amount=2000000`, `activity-amount=1`, `withdrawal-amount=1000000`, and `bootstrap-deposit-amount=10000000` |
| Route and destination | `swap-mechanism=direct-swap`, `swap-liquidity=10000000000`, `recipient-mode=existing`, and `callback-gas-limit=10000000` |
| L1 state | `state-bloat-gib=1` and `force-bloat=false` |
| L1 capacity | `l1-gas-limit=30000000` and `l1-general-gas-limit=30000000` |
| Withdrawal scheduler | `withdrawal-max-batch-gas=30000000`, `withdrawal-max-in-flight-batches=12`, `zone-batch-interval-blocks=120`, and `withdrawal-poll-interval-secs=5` |
| Waiting and drain | `step-timeout=10m`, `setup-settlement-timeout-secs=120`, and `drain-timeout=300` |

For scenario phases, `count` is complete journeys and `tps` is journey starts
per second; `tps` accepts positive decimals so a sustained run can offer load
just above an observed capacity. For the independent `deposit` phase they are
transactions and integer transactions per second. The benchmark-side
`max-concurrent` default is 12 for every phase and preset. It limits txgen
transactions or journeys and is independent of
`withdrawal-max-in-flight-batches`, which limits the Zone withdrawal
scheduler's ordered L1 batches; both happen to default to 12.
`step-timeout` accepts a positive duration ending in
`ms`, `s`, `m`, or `h`; `drain-timeout=0` is the only supported zero-valued
timeout and skips the independent phase's final txpool drain. The workflow
rejects callback gas above 10,000,000, a general L1 limit above the block limit,
and withdrawal batch or planned callback-transaction gas above the general L1
transaction limit. Neobank dispatches also reject swap liquidity outside the
selected mechanism's pinned contract limits or below the selected preset's
required inventory.

The withdrawal setup deposits
`ceil(count / accounts) * deposit-amount` DLUSD to each account and waits for
each exact terminal Zone event. It then waits for the L1 portal's processed
counter to confirm the recorded deposits before running the per-account balance
and allowance preflight. The measured request uses `gasLimit=0`, empty callback
data and `revealTo`, the benchmark account as fallback recipient, and the
Bridge-wallet fixture as the allowlisted L1 recipient. See
`docs/NEOBANK_PRIVATE_ZONE_BENCHMARK.md` for the complete event keys and local
commands.

Repeat runs use the pinned Tempo benchmark's commit-and-feature-keyed MinIO
cache for the `tempo` binary. Cache misses build from source and, when the
runner's MinIO alias is available, populate it through `tempo.nu`. Separate
Cargo target directories under the runner tool cache retain unchanged Zones and
`tempo-xtask` artifacts across clean checkouts. The binary/build cache is
independent of the Schelk L1 baseline: skipping or missing one does not discard
the other. The Schelk cache contains initialized Tempo L1 state only; every run
still creates a new Zone, portal, fee configuration, bootstrap fixture, and
Zone state.

Tempo and txgen are fetched from public repositories at exact commits. Neobank
runs use the repository's existing `EARN_DEPLOY_KEY` to check out
`tempoxyz/earn@main`; the resolved Earn SHA is recorded in the results. No
mnemonic, portal, chain ID, token, target ID, or RPC URL needs to be configured
externally. A runner-local private
mnemonic is generated once when its identity file does not exist. An operator
may instead pre-provision the mode-0600
`zones-benchmark/identity/mnemonic` file under `runner.tool_cache`. The workflow
never selects a public test mnemonic or a public/mainnet write endpoint. Its
mnemonic file, Schelk snapshots, and private sender-auth map are outside the
artifact tree; the sender-auth map is mode 0600 and deleted on exit.

After the workflow exists on the default branch, dispatch it from the Actions
UI or CLI and select the branch/ref to test. This example offers `1.2`
journeys/s with 13 in flight, just above the approximately `1.14` journeys/s
observed at the prior 12-journey capacity point:

```bash
gh workflow run zones-benchmark.yml \
  --ref '<branch-or-tag>' \
  -f phase=neobank-full-journey \
  -f accounts=100 \
  -f count=1000 \
  -f tps=1.2 \
  -f max-concurrent=13 \
  -f state-bloat-gib=1 \
  -f force-bloat=false \
  -f swap-mechanism=direct-swap \
  -f swap-liquidity=10000000000 \
  -f recipient-mode=existing
```

For one-direction smoke runs, change only the phase:

```bash
gh workflow run zones-benchmark.yml --ref '<branch-or-tag>' -f phase=neobank-encrypted-deposit -f accounts=100 -f count=100 -f tps=20 -f max-concurrent=12
gh workflow run zones-benchmark.yml --ref '<branch-or-tag>' -f phase=neobank-private-withdrawal -f accounts=100 -f count=100 -f tps=20 -f max-concurrent=12
```

For an A/B run through the native StablecoinDEX with a fresh ordinary private
transfer destination, change only the two mechanism inputs:

```bash
gh workflow run zones-benchmark.yml \
  --ref '<branch-or-tag>' \
  -f phase=neobank-full-journey \
  -f count=100 \
  -f swap-mechanism=stablecoin-dex \
  -f recipient-mode=random
```

To isolate destination-state reuse in the generic route, dispatch two
otherwise-identical `roundtrip` runs with `recipient-mode=existing` and
`recipient-mode=random`. The mechanism controls are ignored by generic phases.

Set `force-bloat=true` only to replace the paired L1 baseline for the selected
configuration. Ordinary dispatches and PR runs default to `false` and reuse a
matching baseline. This workflow does not provide an `init-only` mode: even
after a rebuild it restores private copies, creates a fresh Zone, and runs the
selected phase.

GitHub does not expose a newly introduced `workflow_dispatch` until the workflow
file exists on the default branch. Before merge, opening, reopening, or pushing
a commit to a same-repository PR whose cumulative diff touches the workflow,
`contrib/bench`, `xtask`, or this document runs `neobank-full-journey` with
100 accounts, 100 journeys, 20 journey starts per second, and 12 maximum in
flight. The authorization job requires both the
triggering actor and PR author to have repository write access and rejects fork
PRs. Newer commits do not cancel an in-progress benchmark; another run for the
same PR queues behind it. Benchmark jobs from different PRs and manual
dispatches also serialize when the shared Schelk host has no free runner. The
scripts can be executed directly on the benchmark host while the workflow is
under review.

One historical generic roundtrip used 200 accounts,
1,000 journeys, 20 journey starts per second, and 200 maximum in flight. It
completed 1,000/1,000 with no failures or timeouts at 13.332 journeys/s, with
12.540 s p50 and 15.208 s p95 journey latency in
[run 29978880031](https://github.com/tempoxyz/zones/actions/runs/29978880031).
The corresponding report is published under the
`zones-roundtrip` scenario.

This is a public repository, and a `pull_request` run evaluates the workflow
definition from the PR. The GitHub repository or organization policy must
require approval for outside-collaborator workflows before they can reach a
self-hosted runner. The same-repository and write-permission checks above are
defense in depth; they do not replace that GitHub-side policy.

Generic `activity` and `withdrawal` are intentionally not dispatch choices
because this self-provisioning workflow starts from a fresh pool with no
pre-existing Zone balance. Run their independent assets manually against a
fixture that passes `--fixture-state funded`, or select `roundtrip` to create
balances within each measured journey. The focused
`neobank-private-withdrawal` choice is self-contained: it creates DLUSD-backed
private EarnShare positions and verifies them outside measurement before
starting the composable redemption load.

### Scenario reporting

The txgen scenario engine accepts one submission URL per named chain. This
workflow submits measured L1 user transactions through validator A and uses
validator B for aggregate queries; unlike the independent deposit pipeline, it
does not spread L1 submissions across both validators.

The provisioned public L1 and Zone endpoints expose WebSocket subscriptions for
observation while transaction submission remains on the configured HTTP RPCs.
Neobank scenario chains use txgen's `auto` observation mode, which prefers
new-head and log subscriptions with canonical backfill and verification, then
falls back to 50 ms HTTP polling. The WebSocket endpoints carry no private-Zone
authorization material.

Scenario mode writes report schema v2 JSON and publishes the same finalized
measured report to ClickHouse. GitHub Actions scenario runs
require `CLICKHOUSE_URL`, `CLICKHOUSE_USER`, and `CLICKHOUSE_PASSWORD`; local
runs retain JSON-only behavior unless those variables are set. Credentials
remain in the process environment, while txgen receives only the
credential-free endpoint and non-secret run/ref metadata. The workflow combines
the JSON report with the rendered scenario to publish a scenario-native results
page. It reports completed journeys per second, aggregate and per-chain
successful submit-step TPS, client-observed end-to-end latency, observed
critical-path latency, causal-edge and chain-inclusion timing, observation lag,
per-step command duration, and receipt gas grouped by chain, input template,
and scenario step. Journey P50/P95/P99 values are calculated from complete
per-instance journeys; they are never produced by summing aggregate step
percentiles. Gas figures come from outer transaction receipts and do not split
internal-call gas. Each measured run retains up to ten txgen lifecycle samples,
which contain transaction and canonical block identities and timing milestones
but exclude calldata, decoded values, credentials, authorization maps, and
signing material. The generated Markdown is also included in the run artifact.

These rates cover the complete measured window, including ramp-up and drain;
they are not a saturation or single-chain capacity claim. Aggregate user TPS
sums successful submit steps across distinct chains. A submit step ends when
the RPC accepts the transaction by default, or when a successful receipt arrives
if the step uses `await: receipt`; receipt and log wait steps report subsequent
execution and cross-chain progress. Untimed bootstrap and approval setup is
excluded. Scenario mode still does not scrape the node metric endpoints used by
Tempo's existing single-chain benchmark harness.

The pinned txgen runtime gives independent same-account DAG submissions
distinct signed expiring-transaction identities through deterministic max-fee
bumps while preserving a zero priority fee. Tempo's reserved expiring nonce key
still encodes numeric nonce zero, so tests compare the signed hashes and fee
identities rather than expecting different numeric nonce fields. Regular-nonce
submissions still require an explicit dependency edge. Reverted withdrawals
and rejected bridge outcomes surface as step timeouts rather than immediate
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
