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
ZONES_BENCH_MAX_CONCURRENT="${ZONES_BENCH_MAX_CONCURRENT:-100}"
ZONES_BENCH_TRANSACTIONS_PER_ACCOUNT="${ZONES_BENCH_TRANSACTIONS_PER_ACCOUNT:-$ZONES_BENCH_COUNT}"
ZONES_BENCH_DEPOSIT_AMOUNT="${ZONES_BENCH_DEPOSIT_AMOUNT:-1000000}"
ZONES_BENCH_ACTIVITY_AMOUNT="${ZONES_BENCH_ACTIVITY_AMOUNT:-1}"
ZONES_BENCH_WITHDRAWAL_AMOUNT="${ZONES_BENCH_WITHDRAWAL_AMOUNT:-1000000}"
ZONES_BENCH_DRAIN_TIMEOUT="${ZONES_BENCH_DRAIN_TIMEOUT:-300}"
ZONES_BENCH_APPROVAL_TIMEOUT_SECS="${ZONES_BENCH_APPROVAL_TIMEOUT_SECS:-40}"
ZONES_BENCH_AUTH_TTL_SECS="${ZONES_BENCH_AUTH_TTL_SECS:-600}"
ZONES_BENCH_AUTH_REFRESH_SECS="${ZONES_BENCH_AUTH_REFRESH_SECS:-60}"
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
    ZONES_BENCH_TRANSACTIONS_PER_ACCOUNT \
    ZONES_BENCH_DEPOSIT_AMOUNT \
    ZONES_BENCH_ACTIVITY_AMOUNT \
    ZONES_BENCH_WITHDRAWAL_AMOUNT
do
    require_positive_uint "$name"
done

if [[ "$phase" != activity ]]; then
    require_positive_uint ZONES_BENCH_APPROVAL_TIMEOUT_SECS
    (( 10#$ZONES_BENCH_APPROVAL_TIMEOUT_SECS > 25 )) ||
        die "approval timeout must exceed the 25-second expiring-nonce window"
    (( 10#$ZONES_BENCH_APPROVAL_TIMEOUT_SECS <= 60 )) ||
        die "approval timeout must not exceed 60 seconds for expiring-nonce setup"
fi

(( 10#$ZONES_BENCH_TRANSACTIONS_PER_ACCOUNT >= 10#$ZONES_BENCH_COUNT )) ||
    die "transactions-per-account must cover the full count because sender selection is random"

txgen_bin="${TXGEN_TEMPO_BIN:-txgen-tempo}"
bench_bin="${TXGEN_BENCH_BIN:-bench}"
command -v "$txgen_bin" >/dev/null || die "txgen-tempo binary not found: $txgen_bin"
command -v "$bench_bin" >/dev/null || die "bench binary not found: $bench_bin"
command -v curl >/dev/null || die "curl is required"
command -v jq >/dev/null || die "jq is required"
if [[ "$phase" != activity ]]; then
    command -v timeout >/dev/null || die "timeout is required"
fi

if [[ -n "${ZONES_XTASK_BIN:-}" ]]; then
    [[ -x "$ZONES_XTASK_BIN" ]] || die "ZONES_XTASK_BIN is not executable: $ZONES_XTASK_BIN"
    preflight_cmd=("$ZONES_XTASK_BIN" benchmark-preflight)
else
    preflight_cmd=(cargo run --profile release -p tempo-xtask -- benchmark-preflight)
fi

mkdir -p "$ZONES_BENCH_OUTPUT" "$(dirname "$ZONES_BENCH_REPORT")"

auth_pid=""
auth_map=""
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
        rm -f -- "$auth_map" "${auth_temp_files[@]}"
        rmdir -- "$secret_dir" 2>/dev/null || true
    fi
    unset ZONES_BENCH_MNEMONIC
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

run_preflight() {
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
        --output "$ZONES_BENCH_OUTPUT" \
        "$@"
}

start_zone_auth_map() {
    local spec="$1"
    local zone_id zone_chain_id auth_deadline auth_ready=0

    zone_id="$(jq -er '.zoneId' "$ZONES_BENCH_OUTPUT/preflight.json")"
    zone_chain_id="$(jq -er '.zoneChainId' "$ZONES_BENCH_OUTPUT/preflight.json")"
    secret_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/zones-benchmark-auth.XXXXXX")"
    chmod 700 "$secret_dir"
    auth_map="$secret_dir/zone-auth.json"
    "$txgen_bin" auth-token-map \
        --spec "$spec" \
        --pool users \
        --zone-id "$zone_id" \
        --chain-id "$zone_chain_id" \
        --ttl-secs "$ZONES_BENCH_AUTH_TTL_SECS" \
        --refresh-before-secs "$ZONES_BENCH_AUTH_REFRESH_SECS" \
        --watch \
        --output "$auth_map" >"$ZONES_BENCH_OUTPUT/auth-token-map.log" 2>&1 &
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
}

run_parallel_approval_setup() {
    local label="$1"
    local spec="$2"
    local query_rpc="$3"
    local submit_rpc="$4"
    local expected="$5"
    shift 5
    local raw="$ZONES_BENCH_OUTPUT/$label-setup.serial.ndjson"
    local parallel="$ZONES_BENCH_OUTPUT/$label-setup.ndjson"
    local actual approval_timeout remaining send_status setup_started
    local -a generate_command=(
        "$txgen_bin" generate
        --spec "$spec"
        --count 0
        --seed "$ZONES_BENCH_SEED"
        --output "$raw"
    )
    local -a send_command=(
        "$bench_bin" send
        --input "$parallel"
        --rpc-url "$submit_rpc"
        --query-rpc-url "$query_rpc"
        --tps 0
        --max-concurrent "$ZONES_BENCH_MAX_CONCURRENT"
        --retries 0
        --drain-timeout 0
        --report console
        "$@"
    )

    if (( 10#$expected == 0 )); then
        echo "untimed $label approvals already satisfy preflight"
        return
    fi
    if [[ -n "${ZONES_BENCH_CPUSET:-}" ]]; then
        generate_command=(taskset --cpu-list "$ZONES_BENCH_CPUSET" "${generate_command[@]}")
        send_command=(taskset --cpu-list "$ZONES_BENCH_CPUSET" "${send_command[@]}")
    fi

    approval_timeout=$((10#$ZONES_BENCH_APPROVAL_TIMEOUT_SECS))
    setup_started=$SECONDS
    echo "$label approval setup: generating $expected expiring-nonce transactions"
    "${generate_command[@]}"
    actual="$(jq -s -r 'length' "$raw")"
    [[ "$actual" == "$expected" ]] ||
        die "$label setup rendered $actual transactions; expected $expected"
    jq -e -s --argjson expected "$expected" '
        length == $expected and
        all(.[];
            .phase == "setup" and
            (.submission_keys | length) == 1 and
            (.inclusion_keys | length) == 1
        ) and
        ([.[].submission_keys[]] | unique | length) == $expected and
        ([.[].inclusion_keys[]] | unique | length) == 1
    ' "$raw" >/dev/null || die "$label setup stream contains invalid scheduling keys"

    jq -c '.inclusion_keys = []' "$raw" >"$parallel"
    jq -e -s --argjson expected "$expected" '
        length == $expected and all(.[]; (.inclusion_keys | length) == 0)
    ' "$parallel" >/dev/null || die "$label setup inclusion barriers were not removed"
    rm -f -- "$raw"

    echo "$label approval setup: sending up to $ZONES_BENCH_MAX_CONCURRENT concurrently"
    remaining=$((approval_timeout - (SECONDS - setup_started)))
    (( remaining > 0 )) ||
        die "$label approval generation exhausted its ${ZONES_BENCH_APPROVAL_TIMEOUT_SECS}s setup window"
    if timeout --foreground --kill-after=5s "${remaining}s" \
        "${send_command[@]}"; then
        send_status=0
    else
        send_status=$?
    fi
    if (( send_status == 124 )); then
        die "$label approval setup exceeded ${ZONES_BENCH_APPROVAL_TIMEOUT_SECS}s; its 25-second expiring nonces can no longer be included"
    fi
    (( send_status == 0 )) || die "$label approval setup failed with status $send_status"
    echo "$label approval setup: $expected/$expected receipts successful in $((SECONDS - setup_started))s"
}

run_preflight

case "$phase" in
    deposit)
        spec="$ZONES_BENCH_OUTPUT/deposit.yml"
        generate_rpc="$L1_RPC_URL"
        query_rpc="$L1_RPC_URL"
        submit_rpc="${ZONES_BENCH_L1_SUBMIT_RPC_URLS:-$L1_RPC_URL}"
        metrics_url="${ZONES_BENCH_L1_METRICS_URL:-}"
        approval_label=portal
        approval_count="$(jq -er '.portalApprovalSetupAccounts | length' \
            "$ZONES_BENCH_OUTPUT/preflight.json")"
        approval_query_rpc="$L1_RPC_URL"
        approval_submit_rpc="$L1_RPC_URL"
        ;;
    activity)
        spec="$ZONES_BENCH_OUTPUT/zone-activity.yml"
        generate_rpc="$ZONE_RPC_URL"
        query_rpc="$ZONE_RPC_URL"
        submit_rpc="$ZONE_PRIVATE_RPC_URL"
        metrics_url="${ZONES_BENCH_ZONE_METRICS_URL:-}"
        ;;
    withdrawal)
        spec="$ZONES_BENCH_OUTPUT/withdrawal.yml"
        generate_rpc="$ZONE_RPC_URL"
        query_rpc="$ZONE_RPC_URL"
        submit_rpc="$ZONE_PRIVATE_RPC_URL"
        metrics_url="${ZONES_BENCH_ZONE_METRICS_URL:-}"
        approval_label=outbox
        approval_count="$(jq -er '.outboxApprovalSetupAccounts | length' \
            "$ZONES_BENCH_OUTPUT/preflight.json")"
        approval_query_rpc="$ZONE_RPC_URL"
        approval_submit_rpc="$ZONE_PRIVATE_RPC_URL"
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
    run_parallel_approval_setup \
        "$approval_label" \
        "$spec" \
        "$approval_query_rpc" \
        "$approval_submit_rpc" \
        "$approval_count" \
        "${sender_auth_args[@]}"

    # Confirm every allowance landed and render a setup-free measured workload.
    run_preflight --no-approval-setup
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
    -m "fixture_state=$ZONES_BENCH_FIXTURE_STATE"
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
