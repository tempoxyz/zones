#!/usr/bin/env bash

set -euo pipefail

die() {
    echo "error: $*" >&2
    exit 1
}

require_env() {
    local name="$1"
    [[ -n "${!name:-}" ]] || die "$name must be set"
}

require_uint() {
    local name="$1"
    local value="${!name:-}"
    [[ "$value" =~ ^[0-9]+$ ]] || die "$name must be an unsigned integer"
}

require_positive_uint() {
    local name="$1"
    require_uint "$name"
    (( 10#${!name} > 0 )) || die "$name must be greater than zero"
}

phase="${1:-}"
case "$phase" in
    deposit | activity | withdrawal) ;;
    *) die "usage: $0 <deposit|activity|withdrawal>" ;;
esac

require_env ZONES_BENCH_MNEMONIC
require_env L1_RPC_URL
require_env ZONE_RPC_URL
require_env ZONES_BENCH_TOKEN
require_env ZONES_BENCH_SEED

for rpc_name in L1_RPC_URL ZONE_RPC_URL; do
    rpc_url="${!rpc_name}"
    [[ "$rpc_url" == http://* || "$rpc_url" == https://* ]] ||
        die "$rpc_name must be an explicit HTTP(S) URL"
done

ZONES_BENCH_ACCOUNT_START="${ZONES_BENCH_ACCOUNT_START:-0}"
ZONES_BENCH_ACCOUNTS="${ZONES_BENCH_ACCOUNTS:-100}"
ZONES_BENCH_COUNT="${ZONES_BENCH_COUNT:-1000}"
ZONES_BENCH_TPS="${ZONES_BENCH_TPS:-100}"
ZONES_BENCH_MAX_CONCURRENT="${ZONES_BENCH_MAX_CONCURRENT:-100}"
ZONES_BENCH_TRANSACTIONS_PER_ACCOUNT="${ZONES_BENCH_TRANSACTIONS_PER_ACCOUNT:-$ZONES_BENCH_COUNT}"
ZONES_BENCH_DEPOSIT_AMOUNT="${ZONES_BENCH_DEPOSIT_AMOUNT:-1000000}"
ZONES_BENCH_ACTIVITY_AMOUNT="${ZONES_BENCH_ACTIVITY_AMOUNT:-1}"
ZONES_BENCH_WITHDRAWAL_AMOUNT="${ZONES_BENCH_WITHDRAWAL_AMOUNT:-1000000}"
ZONES_BENCH_DRAIN_TIMEOUT="${ZONES_BENCH_DRAIN_TIMEOUT:-300}"
ZONES_BENCH_OUTPUT="${ZONES_BENCH_OUTPUT:-target/zones-benchmark/$phase}"
ZONES_BENCH_REPORT="${ZONES_BENCH_REPORT:-target/zones-benchmark/report-$phase.json}"
ZONES_BENCH_FIXTURE_STATE="${ZONES_BENCH_FIXTURE_STATE:-}"

if [[ -z "$ZONES_BENCH_FIXTURE_STATE" ]]; then
    if [[ "$phase" == deposit ]]; then
        ZONES_BENCH_FIXTURE_STATE=empty
    else
        ZONES_BENCH_FIXTURE_STATE=funded
    fi
fi
case "$ZONES_BENCH_FIXTURE_STATE" in
    empty | funded) ;;
    *) die "ZONES_BENCH_FIXTURE_STATE must be empty or funded" ;;
esac

for name in \
    ZONES_BENCH_ACCOUNT_START \
    ZONES_BENCH_SEED \
    ZONES_BENCH_DRAIN_TIMEOUT
do
    require_uint "$name"
done

for name in \
    ZONES_BENCH_ACCOUNTS \
    ZONES_BENCH_COUNT \
    ZONES_BENCH_TPS \
    ZONES_BENCH_MAX_CONCURRENT \
    ZONES_BENCH_TRANSACTIONS_PER_ACCOUNT \
    ZONES_BENCH_DEPOSIT_AMOUNT \
    ZONES_BENCH_ACTIVITY_AMOUNT \
    ZONES_BENCH_WITHDRAWAL_AMOUNT
do
    require_positive_uint "$name"
done

(( 10#$ZONES_BENCH_TRANSACTIONS_PER_ACCOUNT >= 10#$ZONES_BENCH_COUNT )) ||
    die "transactions-per-account must cover the full count because sender selection is random"

txgen_bin="${TXGEN_TEMPO_BIN:-txgen-tempo}"
bench_bin="${TXGEN_BENCH_BIN:-bench}"
command -v "$txgen_bin" >/dev/null || die "txgen-tempo binary not found: $txgen_bin"
command -v "$bench_bin" >/dev/null || die "bench binary not found: $bench_bin"
command -v curl >/dev/null || die "curl is required"
command -v jq >/dev/null || die "jq is required"

if [[ -n "${ZONES_XTASK_BIN:-}" ]]; then
    [[ -x "$ZONES_XTASK_BIN" ]] || die "ZONES_XTASK_BIN is not executable: $ZONES_XTASK_BIN"
    preflight_cmd=("$ZONES_XTASK_BIN" benchmark-preflight)
else
    preflight_cmd=(cargo run --profile release -p tempo-xtask -- benchmark-preflight)
fi

mkdir -p "$ZONES_BENCH_OUTPUT" "$(dirname "$ZONES_BENCH_REPORT")"

"${preflight_cmd[@]}" \
    --l1-rpc-url "$L1_RPC_URL" \
    --zone-rpc-url "$ZONE_RPC_URL" \
    --token "$ZONES_BENCH_TOKEN" \
    --account-start "$ZONES_BENCH_ACCOUNT_START" \
    --accounts "$ZONES_BENCH_ACCOUNTS" \
    --deposit-amount "$ZONES_BENCH_DEPOSIT_AMOUNT" \
    --activity-amount "$ZONES_BENCH_ACTIVITY_AMOUNT" \
    --withdrawal-amount "$ZONES_BENCH_WITHDRAWAL_AMOUNT" \
    --transactions-per-account "$ZONES_BENCH_TRANSACTIONS_PER_ACCOUNT" \
    --check-phase "$phase" \
    --fixture-state "$ZONES_BENCH_FIXTURE_STATE" \
    --output "$ZONES_BENCH_OUTPUT"

case "$phase" in
    deposit)
        spec="$ZONES_BENCH_OUTPUT/deposit.yml"
        generate_rpc="$L1_RPC_URL"
        submit_rpc="${ZONES_BENCH_L1_SUBMIT_RPC_URLS:-$L1_RPC_URL}"
        metrics_url="${ZONES_BENCH_L1_METRICS_URL:-}"
        ;;
    activity)
        spec="$ZONES_BENCH_OUTPUT/zone-activity.yml"
        generate_rpc="$ZONE_RPC_URL"
        submit_rpc="$ZONE_RPC_URL"
        metrics_url="${ZONES_BENCH_ZONE_METRICS_URL:-}"
        ;;
    withdrawal)
        spec="$ZONES_BENCH_OUTPUT/withdrawal.yml"
        generate_rpc="$ZONE_RPC_URL"
        submit_rpc="$ZONE_RPC_URL"
        metrics_url="${ZONES_BENCH_ZONE_METRICS_URL:-}"
        ;;
esac

if (( 10#$ZONES_BENCH_DRAIN_TIMEOUT > 0 )); then
    IFS=',' read -r -a drain_rpcs <<<"$submit_rpc"
    for drain_rpc in "${drain_rpcs[@]}"; do
        txpool_response="$({
            curl --silent --show-error --fail \
                --header 'Content-Type: application/json' \
                --data '{"jsonrpc":"2.0","id":1,"method":"txpool_status","params":[]}' \
                "$drain_rpc"
        } 2>&1)" || die "selected RPC did not answer txpool_status: $drain_rpc"
        if ! jq -e '.error == null and (.result | type == "object")' \
            >/dev/null <<<"$txpool_response"; then
            die "selected RPC does not expose txpool_status: $drain_rpc; expose txpool internally or set ZONES_BENCH_DRAIN_TIMEOUT=0"
        fi
    done
fi

bench_args=(
    send
    --rpc-url "$submit_rpc"
    --tps "$ZONES_BENCH_TPS"
    --max-concurrent "$ZONES_BENCH_MAX_CONCURRENT"
    --retries 0
    --drain-timeout "$ZONES_BENCH_DRAIN_TIMEOUT"
    --scrape-interval-ms 200
    --report "json:$ZONES_BENCH_REPORT"
    -m "job=github-zones-benchmark"
    -m "phase=$phase"
    -m "target_tps=$ZONES_BENCH_TPS"
    -m "accounts=$ZONES_BENCH_ACCOUNTS"
    -m "transaction_count=$ZONES_BENCH_COUNT"
    -m "txgen_seed=$ZONES_BENCH_SEED"
    -m "fixture_state=$ZONES_BENCH_FIXTURE_STATE"
)

zones_revision="${ZONES_BENCH_ZONES_REF:-${GITHUB_SHA:-}}"
if [[ -n "$zones_revision" ]]; then
    bench_args+=(-m "git-sha=$zones_revision")
fi
if [[ -n "${GITHUB_RUN_ID:-}" ]]; then
    bench_args+=(-m "benchmark_id=zones-benchmark-$GITHUB_RUN_ID-${GITHUB_RUN_ATTEMPT:-1}")
fi
if [[ -n "${ZONES_BENCH_TARGET_ID:-}" ]]; then
    bench_args+=(-m "target_id=$ZONES_BENCH_TARGET_ID")
fi
if [[ -n "$metrics_url" ]]; then
    bench_args+=(--metrics-url "$metrics_url")
fi

txgen_command=(
    "$txgen_bin" generate
    --spec "$spec"
    --count "$ZONES_BENCH_COUNT"
    --seed "$ZONES_BENCH_SEED"
    --rpc "$generate_rpc"
)
bench_command=("$bench_bin" "${bench_args[@]}")
if [[ -n "${ZONES_BENCH_CPUSET:-}" ]]; then
    txgen_command=(taskset --cpu-list "$ZONES_BENCH_CPUSET" "${txgen_command[@]}")
    bench_command=(taskset --cpu-list "$ZONES_BENCH_CPUSET" "${bench_command[@]}")
fi

"${txgen_command[@]}" | "${bench_command[@]}"

[[ -s "$ZONES_BENCH_REPORT" ]] || die "bench produced no report at $ZONES_BENCH_REPORT"
if ! jq -e --argjson expected "$ZONES_BENCH_COUNT" \
    '.sent == $expected and .success == $expected and .failed == 0' \
    "$ZONES_BENCH_REPORT" >/dev/null; then
    counts="$(jq -c '{sent, success, failed}' "$ZONES_BENCH_REPORT" 2>/dev/null || true)"
    die "bench did not accept every workload transaction (expected $ZONES_BENCH_COUNT; report ${counts:-unreadable})"
fi
echo "benchmark report: $ZONES_BENCH_REPORT"
