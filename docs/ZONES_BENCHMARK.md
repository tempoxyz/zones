# Zones benchmark transaction generation

This benchmark support prepares three independent `txgen-tempo` workloads for an
L1 deposit, ordinary Zone activity, and a Zone withdrawal. It does not submit a
cross-chain scenario, wait for deposit or withdrawal events, or decide when the
next phase is ready.

## Prerequisites

Start the Tempo L1 and Zone nodes and fund the intended benchmark account range
before running preflight. Install compatible `txgen-tempo` and `bench` binaries
from an explicitly pinned revision of the private `tempoxyz/txgen` repository:

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
export ZONES_BENCH_MNEMONIC='your benchmark mnemonic'
export L1_RPC_URL='your explicit Tempo L1 HTTP RPC URL'
export ZONE_RPC_URL='your explicit Zone HTTP RPC URL'
export TOKEN='the enabled TIP-20 address'
```

There is no fallback mnemonic. Do not use a public test mnemonic for a remotely
writable chain. The commands also require explicit RPC URLs and do not select a
mainnet write endpoint.

`ZONE_RPC_URL` must be a trusted direct/internal RPC endpoint for benchmark
traffic. The public private-RPC endpoint requires an account-specific
`X-Authorization-Token`, while `txgen-tempo` and `bench` do not currently attach
a different header for each sender in a mixed account stream. Do not expose the
direct endpoint publicly.

`tempo-zone dev` funds its default development account, not an arbitrary
mnemonic range. Fund every derived pool address on Tempo and deposit enough of
the selected token into the Zone before checking the corresponding phase;
preflight only reads state and never funds or bridges accounts.

## Run preflight

Preflight derives the account addresses without printing the mnemonic, queries
both chain IDs, resolves the portal and outbox, reads the deposit, bounce-back,
and withdrawal fees, and verifies token state, balances, and allowances for
every account. It rejects an amount that cannot cover protocol charges and the
configured transaction fee budget.

Run all checks and render all workload specs with:

```bash
cargo run -p tempo-xtask -- benchmark-preflight \
  --l1-rpc-url "$L1_RPC_URL" \
  --zone-rpc-url "$ZONE_RPC_URL" \
  --token "$TOKEN" \
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
default is one. Keep each phase's `COUNT` at or below
`accounts * transactions-per-account`, and leave room for random sender skew.
For expiring Zone activity, preflight also budgets txgen's monotonic fee-cap
uniqueness bump through that total transaction capacity.

When an approval is needed, the rendered deposit or withdrawal spec includes it
as a txgen setup transaction. `bench send` submits setup transactions and waits
for their inclusion before it starts workload timing. Keep `--rpc` on the
`txgen-tempo generate` command so these regular-nonce setup transactions start
from the account's current protocol nonce.

## Generate phases independently

The examples below intentionally remain separate. Set `COUNT`, `TPS`, and
`MAX_CONCURRENT` for the desired run.

### Tempo L1 deposit

Generate and stream deposits to the Tempo L1 RPC:

```bash
txgen-tempo generate \
  --spec target/zones-benchmark/deposit.yml \
  --count "$COUNT" \
  --seed 99 \
  --rpc "$L1_RPC_URL" \
| bench send \
  --rpc-url "$L1_RPC_URL" \
  --tps "$TPS" \
  --max-concurrent "$MAX_CONCURRENT"
```

Each transaction deposits to the selected sender's own address, which is in the
same benchmark account pool. Its memo is generated as a fresh random `bytes32`.
Any portal approvals in the stream are setup traffic and are not included in
the measured workload.

### Zone activity

Generate ordinary TIP-20 transfers and stream them directly to the Zone RPC:

```bash
txgen-tempo generate \
  --spec target/zones-benchmark/zone-activity.yml \
  --count "$COUNT" \
  --seed 99 \
  --rpc "$ZONE_RPC_URL" \
| bench send \
  --rpc-url "$ZONE_RPC_URL" \
  --tps "$TPS" \
  --max-concurrent "$MAX_CONCURRENT"
```

The rendered spec uses the queried Zone chain ID and pays gas in the configured
enabled TIP-20. Activity transactions use expiring nonces with a 25-second
validity window, so they must be streamed into `bench send`; do not generate a
file for later replay.

### Zone withdrawal

Generate withdrawal requests and stream them to the Zone RPC:

```bash
txgen-tempo generate \
  --spec target/zones-benchmark/withdrawal.yml \
  --count "$COUNT" \
  --seed 99 \
  --rpc "$ZONE_RPC_URL" \
| bench send \
  --rpc-url "$ZONE_RPC_URL" \
  --tps "$TPS" \
  --max-concurrent "$MAX_CONCURRENT"
```

The workload calls
`requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes,bytes)`
with `gasLimit = 0`, empty callback data, and empty `revealTo`. The Tempo
recipient and Zone fallback recipient are the selected sender's corresponding
account address. Any outbox approvals are untimed setup traffic.

After any phase completes, advance the cross-chain state and confirm readiness
with the existing deployment and inspection commands before running the next
phase. Event waiting and L1→Zone→L1 orchestration are deliberately outside this
change.
