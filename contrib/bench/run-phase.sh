#!/usr/bin/env bash

set -Eeuo pipefail

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

load_benchmark_mnemonic() {
    local mode

    require_env ZONES_BENCH_MNEMONIC_FILE
    [[ ! -L "$ZONES_BENCH_MNEMONIC_FILE" ]] ||
        die "ZONES_BENCH_MNEMONIC_FILE must not be a symbolic link"
    [[ -f "$ZONES_BENCH_MNEMONIC_FILE" ]] ||
        die "ZONES_BENCH_MNEMONIC_FILE must be a regular file"
    [[ -s "$ZONES_BENCH_MNEMONIC_FILE" ]] ||
        die "ZONES_BENCH_MNEMONIC_FILE must not be empty"
    [[ -r "$ZONES_BENCH_MNEMONIC_FILE" ]] ||
        die "ZONES_BENCH_MNEMONIC_FILE must be readable"

    mode="$(stat -c '%a' -- "$ZONES_BENCH_MNEMONIC_FILE")"
    [[ "$mode" =~ ^[0-7]{3,4}$ ]] ||
        die "could not validate ZONES_BENCH_MNEMONIC_FILE permissions"
    (( (8#$mode & 8#077) == 0 )) ||
        die "ZONES_BENCH_MNEMONIC_FILE must not be accessible by group or other users"

    ZONES_BENCH_MNEMONIC="$(<"$ZONES_BENCH_MNEMONIC_FILE")"
    [[ "$ZONES_BENCH_MNEMONIC" =~ [^[:space:]] ]] ||
        die "ZONES_BENCH_MNEMONIC_FILE must contain a mnemonic"
    export ZONES_BENCH_MNEMONIC
}

phase="${1:-}"
case "$phase" in
    deposit | activity | withdrawal) ;;
    *) die "usage: $0 <deposit|activity|withdrawal>" ;;
esac

load_benchmark_mnemonic
require_env L1_RPC_URL
require_env ZONE_RPC_URL
require_env ZONES_BENCH_TOKEN
require_env ZONES_BENCH_SEED
for name in \
    L1_PORTAL_ADDRESS ZONES_BENCH_EXPECTED_L1_CHAIN_ID \
    ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID ZONES_BENCH_EXPECTED_ZONE_ID \
    ZONES_BENCH_INBOX ZONES_BENCH_OUTBOX \
    ZONES_BENCH_L1_MAX_FEE_PER_GAS ZONES_BENCH_L1_MAX_PRIORITY_FEE_PER_GAS \
    ZONES_BENCH_ZONE_MAX_FEE_PER_GAS ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS \
    ZONES_BENCH_ACCOUNT_END ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX \
    ZONES_BENCH_SEQUENCER_ACCOUNT_END
do
    require_env "$name"
done
if [[ "$phase" != deposit ]]; then
    require_env ZONE_PRIVATE_RPC_URL
fi

rpc_names=(L1_RPC_URL ZONE_RPC_URL)
if [[ "$phase" != deposit ]]; then
    rpc_names+=(ZONE_PRIVATE_RPC_URL)
fi
for rpc_name in "${rpc_names[@]}"; do
    rpc_url="${!rpc_name}"
    [[ "$rpc_url" == http://* || "$rpc_url" == https://* ]] ||
        die "$rpc_name must be an explicit HTTP(S) URL"
done

ZONES_BENCH_ACCOUNT_START="${ZONES_BENCH_ACCOUNT_START:-0}"
ZONES_BENCH_ACCOUNTS="${ZONES_BENCH_ACCOUNTS:-100}"
ZONES_BENCH_COUNT="${ZONES_BENCH_COUNT:-100}"
ZONES_BENCH_TPS="${ZONES_BENCH_TPS:-100}"
ZONES_BENCH_MAX_CONCURRENT="${ZONES_BENCH_MAX_CONCURRENT:-12}"
ZONES_BENCH_DEPOSIT_AMOUNT="${ZONES_BENCH_DEPOSIT_AMOUNT:-1000000}"
ZONES_BENCH_ACTIVITY_AMOUNT="${ZONES_BENCH_ACTIVITY_AMOUNT:-1}"
ZONES_BENCH_WITHDRAWAL_AMOUNT="${ZONES_BENCH_WITHDRAWAL_AMOUNT:-1000000}"
ZONES_BENCH_APPROVAL_GAS_LIMIT="${ZONES_BENCH_APPROVAL_GAS_LIMIT:-2000000}"
ZONES_BENCH_DEPOSIT_GAS_LIMIT="${ZONES_BENCH_DEPOSIT_GAS_LIMIT:-2000000}"
ZONES_BENCH_ACTIVITY_GAS_LIMIT="${ZONES_BENCH_ACTIVITY_GAS_LIMIT:-500000}"
ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT="${ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT:-10000000}"
ZONES_BENCH_DRAIN_TIMEOUT="${ZONES_BENCH_DRAIN_TIMEOUT:-300}"
ZONES_BENCH_RECIPIENT_MODE="${ZONES_BENCH_RECIPIENT_MODE:-existing}"
ZONES_BENCH_AUTH_TTL_SECS="${ZONES_BENCH_AUTH_TTL_SECS:-600}"
ZONES_BENCH_AUTH_REFRESH_SECS="${ZONES_BENCH_AUTH_REFRESH_SECS:-60}"
ZONES_BENCH_OUTPUT="${ZONES_BENCH_OUTPUT:-target/zones-benchmark/$phase}"
ZONES_BENCH_REPORT="${ZONES_BENCH_REPORT:-target/zones-benchmark/report-$phase.json}"
case "$ZONES_BENCH_RECIPIENT_MODE" in
    existing)
        ZONES_BENCH_RECIPIENT_GENERATOR='{ pool: { pool: users, select: random } }'
        ;;
    random)
        ZONES_BENCH_RECIPIENT_GENERATOR=random
        ;;
    *) die "ZONES_BENCH_RECIPIENT_MODE must be existing or random" ;;
esac
ZONES_BENCH_RECIPIENT_ACCOUNT_START="$ZONES_BENCH_ACCOUNT_START"
ZONES_BENCH_RECIPIENT_ACCOUNT_END="$ZONES_BENCH_ACCOUNT_END"
ZONES_BENCH_RECIPIENT_POOL=users
ZONES_BENCH_RECIPIENT_SELECT=random
ZONES_BENCH_L1_QUERY_RPC_URL="${ZONES_BENCH_L1_QUERY_RPC_URL:-$L1_RPC_URL}"
[[ "$ZONES_BENCH_L1_QUERY_RPC_URL" == http://* || "$ZONES_BENCH_L1_QUERY_RPC_URL" == https://* ]] ||
    die "ZONES_BENCH_L1_QUERY_RPC_URL must be an explicit HTTP(S) URL"

for name in \
    ZONES_BENCH_ACCOUNT_START \
    ZONES_BENCH_ACCOUNT_END \
    ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX \
    ZONES_BENCH_SEQUENCER_ACCOUNT_END \
    ZONES_BENCH_SEED \
    ZONES_BENCH_DRAIN_TIMEOUT \
    ZONES_BENCH_L1_MAX_PRIORITY_FEE_PER_GAS \
    ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS
do
    require_uint "$name"
done

if [[ "$phase" != deposit ]]; then
    for name in ZONES_BENCH_AUTH_TTL_SECS ZONES_BENCH_AUTH_REFRESH_SECS; do
        require_positive_uint "$name"
    done
    (( 10#$ZONES_BENCH_AUTH_REFRESH_SECS < 10#$ZONES_BENCH_AUTH_TTL_SECS )) ||
        die "auth refresh lead time must be below the token TTL"
fi

for name in \
    ZONES_BENCH_ACCOUNTS \
    ZONES_BENCH_COUNT \
    ZONES_BENCH_TPS \
    ZONES_BENCH_MAX_CONCURRENT \
    ZONES_BENCH_DEPOSIT_AMOUNT \
    ZONES_BENCH_ACTIVITY_AMOUNT \
    ZONES_BENCH_WITHDRAWAL_AMOUNT \
    ZONES_BENCH_APPROVAL_GAS_LIMIT \
    ZONES_BENCH_DEPOSIT_GAS_LIMIT \
    ZONES_BENCH_ACTIVITY_GAS_LIMIT \
    ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT \
    ZONES_BENCH_EXPECTED_L1_CHAIN_ID \
    ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID \
    ZONES_BENCH_EXPECTED_ZONE_ID \
    ZONES_BENCH_L1_MAX_FEE_PER_GAS \
    ZONES_BENCH_ZONE_MAX_FEE_PER_GAS
do
    require_positive_uint "$name"
done

(( 10#$ZONES_BENCH_ACCOUNT_END - 10#$ZONES_BENCH_ACCOUNT_START == 10#$ZONES_BENCH_ACCOUNTS )) ||
    die "topology account range does not match ZONES_BENCH_ACCOUNTS"

export \
    L1_PORTAL_ADDRESS ZONES_BENCH_TOKEN \
    ZONES_BENCH_EXPECTED_L1_CHAIN_ID ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID \
    ZONES_BENCH_EXPECTED_ZONE_ID ZONES_BENCH_INBOX ZONES_BENCH_OUTBOX \
    ZONES_BENCH_L1_MAX_FEE_PER_GAS ZONES_BENCH_L1_MAX_PRIORITY_FEE_PER_GAS \
    ZONES_BENCH_ZONE_MAX_FEE_PER_GAS ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS \
    ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX ZONES_BENCH_SEQUENCER_ACCOUNT_END \
    ZONES_BENCH_ACCOUNT_START ZONES_BENCH_ACCOUNT_END \
    ZONES_BENCH_DEPOSIT_AMOUNT ZONES_BENCH_ACTIVITY_AMOUNT ZONES_BENCH_WITHDRAWAL_AMOUNT \
    ZONES_BENCH_APPROVAL_GAS_LIMIT ZONES_BENCH_DEPOSIT_GAS_LIMIT \
    ZONES_BENCH_ACTIVITY_GAS_LIMIT ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT \
    ZONES_BENCH_RECIPIENT_GENERATOR ZONES_BENCH_RECIPIENT_ACCOUNT_START \
    ZONES_BENCH_RECIPIENT_ACCOUNT_END ZONES_BENCH_RECIPIENT_POOL \
    ZONES_BENCH_RECIPIENT_SELECT ZONES_BENCH_L1_QUERY_RPC_URL

txgen_bin="${TXGEN_TEMPO_BIN:-txgen-tempo}"
bench_bin="${TXGEN_BENCH_BIN:-bench}"
command -v "$txgen_bin" >/dev/null || die "txgen-tempo binary not found: $txgen_bin"
command -v "$bench_bin" >/dev/null || die "bench binary not found: $bench_bin"
command -v curl >/dev/null || die "curl is required"
command -v jq >/dev/null || die "jq is required"

mkdir -p "$ZONES_BENCH_OUTPUT" "$(dirname "$ZONES_BENCH_REPORT")"
spec_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/txgen" && pwd)"

auth_pid=""
auth_map=""
auth_log=""
secret_dir=""
cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if [[ -n "$auth_pid" ]]; then
        if kill -0 "$auth_pid" 2>/dev/null; then
            kill -TERM "$auth_pid" 2>/dev/null || true
        fi
        wait "$auth_pid" 2>/dev/null || true
    fi
    if [[ -n "$secret_dir" && "$secret_dir" == "${RUNNER_TEMP:-/tmp}/zones-benchmark-auth."* ]]; then
        local -a auth_temp_files=()
        shopt -s nullglob
        auth_temp_files=("$secret_dir"/.zone-auth.json.txgen-*.tmp)
        shopt -u nullglob
        rm -f -- "$auth_map" "$auth_log" "${auth_temp_files[@]}"
        rmdir -- "$secret_dir" 2>/dev/null || true
    fi
    unset ZONES_BENCH_MNEMONIC
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

start_zone_auth_map() {
    local spec="$1"
    local auth_deadline auth_ready=0

    secret_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/zones-benchmark-auth.XXXXXX")"
    chmod 700 "$secret_dir"
    auth_map="$secret_dir/zone-auth.json"
    auth_log="$secret_dir/auth-token-map.log"
    "$txgen_bin" auth-token-map \
        --spec "$spec" \
        --pool users \
        --zone-id "$ZONES_BENCH_EXPECTED_ZONE_ID" \
        --chain-id "$ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID" \
        --ttl-secs "$ZONES_BENCH_AUTH_TTL_SECS" \
        --refresh-before-secs "$ZONES_BENCH_AUTH_REFRESH_SECS" \
        --watch \
        --output "$auth_map" >"$auth_log" 2>&1 &
    auth_pid=$!

    auth_deadline=$((SECONDS + 60))
    while (( SECONDS < auth_deadline )); do
        if [[ -f "$auth_map" ]] \
            && [[ "$(stat -c '%a' "$auth_map")" == 600 ]] \
            && jq -e --argjson expected "$ZONES_BENCH_ACCOUNTS" \
                'type == "object" and length == $expected' "$auth_map" >/dev/null 2>&1; then
            auth_ready=1
            break
        fi
        kill -0 "$auth_pid" 2>/dev/null || die "Zone auth-token map process exited during startup"
        sleep 1
    done
    (( auth_ready == 1 )) || die "timed out waiting for a complete Zone auth-token map"
    export ZONES_BENCH_ZONE_AUTH_MAP="$auth_map"
}

run_approval_scenario() {
    local label="$1"
    local scenario="$2"
    local report="$ZONES_BENCH_OUTPUT/$label-approval-report.json"
    local -a command=(
        "$txgen_bin" scenario run
        --scenario "$scenario"
        --count "$ZONES_BENCH_ACCOUNTS"
        --starts-per-second 0
        --max-in-flight "$ZONES_BENCH_MAX_CONCURRENT"
        --max-rpc-in-flight "$ZONES_BENCH_MAX_CONCURRENT"
        --failure-policy fail-fast
        --seed "$ZONES_BENCH_SEED"
        --report "$report"
    )
    if [[ -n "${ZONES_BENCH_CPUSET:-}" ]]; then
        command=(taskset --cpu-list "$ZONES_BENCH_CPUSET" "${command[@]}")
    fi
    echo "$label approval setup: running $ZONES_BENCH_ACCOUNTS receipt-checked transactions"
    "${command[@]}"
    jq -e --argjson expected "$ZONES_BENCH_ACCOUNTS" \
        '.started == $expected and .completed == $expected and .failed == 0 and .timed_out == 0' \
        "$report" >/dev/null ||
        die "$label approval setup did not complete for every benchmark account"
}

case "$phase" in
    deposit)
        spec="$spec_dir/deposit.yml"
        generate_rpc="$L1_RPC_URL"
        query_rpc="$L1_RPC_URL"
        submit_rpc="${ZONES_BENCH_L1_SUBMIT_RPC_URLS:-$L1_RPC_URL}"
        metrics_url="${ZONES_BENCH_L1_METRICS_URL:-}"
        approval_label=portal
        approval_scenario="$spec_dir/portal-approval-scenario.yml"
        ;;
    activity)
        spec="$spec_dir/zone-activity.yml"
        generate_rpc="$ZONE_RPC_URL"
        query_rpc="$ZONE_RPC_URL"
        submit_rpc="$ZONE_PRIVATE_RPC_URL"
        metrics_url="${ZONES_BENCH_ZONE_METRICS_URL:-}"
        ;;
    withdrawal)
        spec="$spec_dir/withdrawal.yml"
        generate_rpc="$ZONE_RPC_URL"
        query_rpc="$ZONE_RPC_URL"
        submit_rpc="$ZONE_PRIVATE_RPC_URL"
        metrics_url="${ZONES_BENCH_ZONE_METRICS_URL:-}"
        approval_label=outbox
        approval_scenario="$spec_dir/outbox-approval-scenario.yml"
        ;;
esac

sender_auth_args=()
if [[ "$phase" != deposit ]]; then
    start_zone_auth_map "$spec"
    sender_auth_args=(
        --sender-header-name X-Authorization-Token
        --sender-header-map "$auth_map"
    )
fi

if [[ "$phase" == deposit || "$phase" == withdrawal ]]; then
    run_approval_scenario "$approval_label" "$approval_scenario"
fi

if (( 10#$ZONES_BENCH_DRAIN_TIMEOUT > 0 )); then
    txpool_response="$({
        curl --silent --show-error --fail \
            --header 'Content-Type: application/json' \
            --data '{"jsonrpc":"2.0","id":1,"method":"txpool_status","params":[]}' \
            "$query_rpc"
    } 2>&1)" || die "query RPC did not answer txpool_status: $query_rpc"
    if ! jq -e '.error == null and (.result | type == "object")' \
        >/dev/null <<<"$txpool_response"; then
        die "query RPC does not expose txpool_status: $query_rpc; expose txpool internally or set ZONES_BENCH_DRAIN_TIMEOUT=0"
    fi
fi

bench_args=(
    send
    --rpc-url "$submit_rpc"
    --query-rpc-url "$query_rpc"
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
)
bench_args+=("${sender_auth_args[@]}")

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
