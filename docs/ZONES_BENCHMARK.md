# Zones benchmark transaction generation

This benchmark support prepares three independent `txgen-tempo` workloads for an
L1 deposit, ordinary Zone activity, and a Zone withdrawal. It does not submit a
cross-chain scenario, wait for deposit or withdrawal events, or decide when the
next phase is ready.

## Production-shaped benchmark environment

Do not use Anvil or `tempo-zone dev` for performance results. The closest match
to Tempo's e2e benchmark is an isolated Tempo consensus network plus a genuinely
deployed, persistent Zone:

1. Run at least two real Tempo validator processes with separate databases,
   consensus identities, RPC endpoints, and metrics endpoints. Tempo's
   `bench-e2e.nu` generates the localnet with `tempo-xtask generate-localnet`,
   starts two validators, waits for peering and chain progress, and restores
   their databases from Schelk snapshots between comparison runs.
2. Deploy a compatible ZoneFactory and create a real ZonePortal on that L1. Use
   separate factory owner, portal admin, and hot sequencer identities.
3. Retain the generated Zone genesis and metadata as deployment artifacts. Run
   the release or profiling `tempo-zone node` binary with a durable datadir.
4. Run txgen on separate benchmark-runner capacity. Send Tempo deposits to the
   L1 validator RPCs and Zone activity/withdrawals to the sequencer's trusted
   internal RPC.

Tempo's `generate-localnet` creates pathUSD, AlphaUSD, BetaUSD, and ThetaUSD at
their reserved addresses and mints each generated account a large balance in
genesis. Its e2e harness generates at least `accounts + 1` signable accounts and
can run `generate-state-bloat --token <id>` before database initialization to
add TIP-20 storage. With bloat loaded, it skips the test faucet because the
txgen accounts are already funded in the snapshot.

A Zones benchmark should pass a secret benchmark mnemonic explicitly when
generating its isolated Tempo L1. Use separate role mnemonics, or reserve
non-overlapping indices and set the benchmark `account-start` after the
validator and Zone operator roles. This makes the configured pool funded on L1
without using `tempo_fundAddress`. Zone balances must still be created through
real deposits and backed by the portal escrow. Do not pre-mint synthetic Zone
balances for a withdrawal benchmark; that would measure an insolvent bridge
state.

After the desired deposits have been ingested, take coordinated L1 and Zone
database snapshots for repeatable activity or withdrawal runs. Creating those
deposits, waiting for ingestion, and coordinating snapshots is the scenario
runner intentionally excluded from this change.

Inject `SEQUENCER_KEY` into the service environment from the deployment secret
manager. The Zone sequencer should use a command shaped like:

```bash
target/release/tempo-zone node \
  --chain /etc/tempo-zone/benchmark/genesis.json \
  --datadir /var/lib/tempo-zone/benchmark \
  --l1.rpc-url "$L1_WS_RPC_URL" \
  --l1.portal-address "$PORTAL" \
  --l1.genesis-block-number "$TEMPO_ANCHOR_BLOCK" \
  --zone.id "$ZONE_ID" \
  --http \
  --http.addr "$TRUSTED_RPC_BIND_ADDR" \
  --http.port 8546 \
  --http.api eth,net,web3,txpool \
  --private-rpc.port 8544 \
  --metrics "$TRUSTED_RPC_BIND_ADDR:9201" \
  --log.file.directory /var/log/tempo-zone/benchmark \
  --sequencer
```

Set `TRUSTED_RPC_BIND_ADDR` to a private interface reachable only from the
sequencer host and benchmark runner; use loopback when a private proxy or tunnel
provides that path. The L1 WebSocket must support subscriptions and settlement
transaction submission. The sequencer key signs Zone blocks and L1
batch/withdrawal transactions, so it must match the portal sequencer and hold
enough L1 fee token. Keep the regular Zone HTTP RPC on a trusted network. The
authenticated private RPC is the public client path, but the txgen/bench
revision pinned by this workflow cannot attach a different
`X-Authorization-Token` for every sender in a mixed-account stream.

`bench send` uses `txpool_status` for its default drain check. The selected
trusted RPC must expose the `txpool` module, or the benchmark must explicitly use
a zero drain timeout. Every generated workload still waits for its transaction
receipt, but zero skips the final global pool-empty check and weakens isolation
between consecutive runs. The workflow and runner script validate
`txpool_status` before sending when the drain timeout is non-zero.

This is production-shaped rather than production-equivalent. Zones is currently
documented as testnet-only, the optional multi-sequencer topology has a static
leader with no automatic promotion, and proof generation is not final.

### Fresh local provisioning status

There is no checked-in, self-contained production-equivalent provisioner for
this topology. The current ZonePortal accepts only the canonical TIP-1091
factory address, while the Tempo revision pinned by Zones does not implement the
proposed native factory.

A production-shaped reference-contract localnet is nevertheless possible
without Anvil. Generate the two-validator Tempo localnet first, then, before
either validator database is initialized, install the reference Solidity
ZoneFactory at the canonical address in the shared genesis. The existing
`install_reference_zone_factory` helper in `crates/node/tests/it/utils.rs` shows
the required constructor-equivalent accounts and storage: factory runtime,
nonce and storage, plus the Verifier and ZoneMessenger runtimes. A standalone
command has not yet been promoted from that test helper. After installing it,
initialize both validators from the identical genesis, call `create-zone`, and
run the release Zone node against the resulting portal and L1 anchor.

That setup exercises real Tempo consensus, portal escrow, L1 ingestion,
settlement submission, and the Zone runtime. It is not an exact implementation
of the proposed native TIP-1091 factory lifecycle and ABI: the reference factory
deploys full portal bytecode, while the current proposal uses a proxy to a
protocol-managed implementation, so L1 portal gas is not final-production
exact. The phase workflow in this change therefore targets a compatible,
externally provisioned persistent or snapshot-backed Zone rather than silently
substituting the reference factory. Do not fall back to `tempo-zone dev`, deploy
at a non-canonical factory address, or insert unbacked Zone balances to make a
run green.

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
- checks token balances and allowances for every benchmark account;
- rejects phase amounts that do not cover protocol and transaction fee budgets;
- renders all three txgen specs and injects any required untimed approvals; and
- writes a non-secret `preflight.json` report.

It does not create a Zone, fund an account, submit an approval or workload
transaction, wait for cross-chain state, or run txgen. `tempo-xtask create-zone`
is separate deployment tooling and is not called by this benchmark workflow.

## Prerequisites

Install compatible `txgen-tempo` and `bench` binaries from an explicitly pinned
revision of the private `tempoxyz/txgen` repository:

```bash
export TXGEN_REV='<approved txgen commit>'
cargo install --git https://github.com/tempoxyz/txgen \
  --rev "$TXGEN_REV" --locked txgen-tempo bench-cli
```

The installed Git credential helper must have access to that repository. The
render/generation compatibility test can be run with:

```bash
TXGEN_TEMPO_BIN="$(command -v txgen-tempo)" \
  cargo test -p tempo-xtask \
  txgen_generates_representative_local_transactions_when_installed
```

Set the account mnemonic only through the environment:

```bash
export ZONES_BENCH_MNEMONIC='your secret benchmark mnemonic'
export L1_RPC_URL='your explicit Tempo L1 HTTP RPC URL'
export ZONE_RPC_URL='your explicit trusted Zone HTTP RPC URL'
export ZONES_BENCH_TOKEN='the enabled TIP-20 address'
```

There is no fallback mnemonic. Do not use a public test mnemonic for a remotely
writable chain. The commands also require explicit RPC URLs and do not select a
mainnet or public testnet write endpoint.

Fund every derived pool address on Tempo before the deposit phase. Use real
deposits to prepare the corresponding Zone balances before the activity or
withdrawal phase. Preflight fails instead of silently funding or bridging an
account.

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
  --check-phase all \
  --output target/zones-benchmark
```

Pass `--zone-dir generated/<zone>` when generated Zone metadata should be used
to resolve and cross-check the deployed addresses. Use `--check-phase deposit`,
`activity`, or `withdrawal` to enforce only that phase's capacity after manually
advancing cross-chain state. Preflight always queries and reports both networks
and renders all three specs, but injects approval setup only for the selected
phase. The output directory contains `deposit.yml`, `zone-activity.yml`,
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
private bare-metal runner class used by Tempo's e2e benchmark. Configure a
protected GitHub environment named `zones-benchmark` with:

| Kind | Name | Purpose |
| --- | --- | --- |
| Secret | `ZONES_BENCH_L1_RPC_URL` | Explicit L1 HTTP write RPC |
| Secret | `ZONES_BENCH_ZONE_RPC_URL` | Trusted internal Zone HTTP write RPC |
| Secret | `ZONES_BENCH_MNEMONIC` | Benchmark account pool |
| Secret | `DEREK_BENCH_TOKEN` | Read access to private `tempoxyz/txgen` |
| Variable | `ZONES_BENCH_TOKEN` | Enabled TIP-20 and fee token address |
| Variable | `ZONES_BENCH_TARGET_ID` | Versioned L1, Zone, and snapshot identity recorded in the report |
| Variable | `ZONES_BENCH_PORTAL` | Expected ZonePortal address on L1 |
| Variable | `ZONES_BENCH_L1_CHAIN_ID` | Expected Tempo L1 chain ID |
| Variable | `ZONES_BENCH_ZONE_CHAIN_ID` | Expected Zone chain ID |
| Variable | `ZONES_BENCH_ZONE_ID` | Expected portal Zone ID |
| Secret, optional | `ZONES_BENCH_L1_METRICS_URL` | L1 metrics target passed to bench |
| Secret, optional | `ZONES_BENCH_ZONE_METRICS_URL` | Zone metrics target passed to bench |

The runner must reach the trusted internal RPC and metrics endpoints. Protect the
environment with required reviewers because an approved run executes the
selected revision's benchmark code with the mnemonic and write endpoints. Set a
new target ID whenever either node build or the restored coordinated snapshot
changes; the workflow records it as benchmark metadata but cannot verify the
external deployment itself.

After the workflow exists on the default branch, dispatch it from the Actions
UI or CLI and select the branch/ref to test:

```bash
gh workflow run zones-benchmark.yml \
  --ref '<branch-or-tag>' \
  -f phase=activity \
  -f accounts=100 \
  -f count=1000 \
  -f tps=100
```

GitHub does not expose a newly introduced `workflow_dispatch` until the workflow
file exists on the default branch. For a same-repository draft PR, create and
apply one of `zones-benchmark-deposit`, `zones-benchmark-activity`, or
`zones-benchmark-withdrawal` to opt into the protected workflow's PR-label
trigger. The label path uses the workflow defaults. The phase runner can also be
executed directly on the benchmark host while the workflow is under review.

Every run uploads its rendered specs, `preflight.json`, and bench JSON report.
Activity and withdrawal deliberately fail preflight until an operator has made
the benchmark pool ready through real cross-chain state transitions.
