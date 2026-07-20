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

for name in \
    ZONES_BENCH_MNEMONIC L1_RPC_URL ZONE_RPC_URL ZONE_PRIVATE_RPC_URL \
    L1_PORTAL_ADDRESS ZONES_BENCH_TOKEN ZONES_BENCH_SEED
do
    require_env "$name"
done

for name in L1_RPC_URL ZONE_RPC_URL ZONE_PRIVATE_RPC_URL; do
    rpc_url="${!name}"
    [[ "$rpc_url" == http://* || "$rpc_url" == https://* ]] ||
        die "$name must be an explicit HTTP(S) URL"
done

ZONES_BENCH_L1_QUERY_RPC_URL="${ZONES_BENCH_L1_QUERY_RPC_URL:-$L1_RPC_URL}"
[[ "$ZONES_BENCH_L1_QUERY_RPC_URL" == http://* || "$ZONES_BENCH_L1_QUERY_RPC_URL" == https://* ]] ||
    die "ZONES_BENCH_L1_QUERY_RPC_URL must be an explicit HTTP(S) URL"
export ZONES_BENCH_L1_QUERY_RPC_URL
ZONES_BENCH_ACCOUNT_START="${ZONES_BENCH_ACCOUNT_START:-16}"
ZONES_BENCH_ACCOUNTS="${ZONES_BENCH_ACCOUNTS:-100}"
ZONES_BENCH_COUNT="${ZONES_BENCH_COUNT:-100}"
ZONES_BENCH_TPS="${ZONES_BENCH_TPS:-10}"
ZONES_BENCH_MAX_CONCURRENT="${ZONES_BENCH_MAX_CONCURRENT:-100}"
ZONES_BENCH_DEPOSIT_AMOUNT="${ZONES_BENCH_DEPOSIT_AMOUNT:-2000000}"
ZONES_BENCH_ACTIVITY_AMOUNT="${ZONES_BENCH_ACTIVITY_AMOUNT:-1}"
ZONES_BENCH_WITHDRAWAL_AMOUNT="${ZONES_BENCH_WITHDRAWAL_AMOUNT:-1000000}"
ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT="${ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT:-10000000}"
ZONES_BENCH_OUTPUT="${ZONES_BENCH_OUTPUT:-target/zones-benchmark/roundtrip}"
ZONES_BENCH_REPORT="${ZONES_BENCH_REPORT:-target/zones-benchmark/report-roundtrip.json}"
ZONES_BENCH_BOOTSTRAP_REPORT="${ZONES_BENCH_BOOTSTRAP_REPORT:-target/zones-benchmark/report-bootstrap.json}"
ZONES_BENCH_STEP_TIMEOUT="${ZONES_BENCH_STEP_TIMEOUT:-10m}"
ZONES_BENCH_AUTH_TTL_SECS="${ZONES_BENCH_AUTH_TTL_SECS:-600}"
ZONES_BENCH_AUTH_REFRESH_SECS="${ZONES_BENCH_AUTH_REFRESH_SECS:-60}"
ZONES_BENCH_SAMPLE_INSTANCES="${ZONES_BENCH_SAMPLE_INSTANCES:-10}"

for name in ZONES_BENCH_ACCOUNT_START ZONES_BENCH_SEED; do
    require_uint "$name"
done
for name in \
    ZONES_BENCH_ACCOUNTS ZONES_BENCH_COUNT ZONES_BENCH_TPS \
    ZONES_BENCH_MAX_CONCURRENT ZONES_BENCH_DEPOSIT_AMOUNT \
    ZONES_BENCH_ACTIVITY_AMOUNT ZONES_BENCH_WITHDRAWAL_AMOUNT \
    ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT ZONES_BENCH_AUTH_TTL_SECS \
    ZONES_BENCH_AUTH_REFRESH_SECS ZONES_BENCH_SAMPLE_INSTANCES
do
    require_positive_uint "$name"
done

(( 10#$ZONES_BENCH_MAX_CONCURRENT <= 10#$ZONES_BENCH_ACCOUNTS )) ||
    die "max-concurrent cannot exceed accounts for an exclusively leased roundtrip pool"
(( 10#$ZONES_BENCH_AUTH_REFRESH_SECS < 10#$ZONES_BENCH_AUTH_TTL_SECS )) ||
    die "auth refresh lead time must be below the token TTL"

journeys_per_account=$(((10#$ZONES_BENCH_COUNT + 10#$ZONES_BENCH_ACCOUNTS - 1) / 10#$ZONES_BENCH_ACCOUNTS))

txgen_bin="${TXGEN_TEMPO_BIN:-txgen-tempo}"
command -v "$txgen_bin" >/dev/null || die "txgen-tempo binary not found: $txgen_bin"
command -v cast >/dev/null || die "cast is required"
command -v jq >/dev/null || die "jq is required"

if [[ -n "${ZONES_XTASK_BIN:-}" ]]; then
    [[ -x "$ZONES_XTASK_BIN" ]] || die "ZONES_XTASK_BIN is not executable: $ZONES_XTASK_BIN"
    preflight_cmd=("$ZONES_XTASK_BIN" benchmark-preflight)
    fee_cmd=("$ZONES_XTASK_BIN" configure-benchmark-fees)
else
    preflight_cmd=(cargo run --profile release -p tempo-xtask -- benchmark-preflight)
    fee_cmd=(cargo run --profile release -p tempo-xtask -- configure-benchmark-fees)
fi

mkdir -p "$ZONES_BENCH_OUTPUT" "$(dirname "$ZONES_BENCH_REPORT")" \
    "$(dirname "$ZONES_BENCH_BOOTSTRAP_REPORT")"

auth_pid=""
progress_pid=""
secret_dir=""
cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if [[ -n "$progress_pid" ]] && kill -0 "$progress_pid" 2>/dev/null; then
        kill -TERM "$progress_pid" 2>/dev/null || true
        wait "$progress_pid" 2>/dev/null || true
    fi
    if [[ -n "$auth_pid" ]] && kill -0 "$auth_pid" 2>/dev/null; then
        kill -TERM "$auth_pid" 2>/dev/null || true
        wait "$auth_pid" 2>/dev/null || true
    fi
    if [[ -n "$secret_dir" && "$secret_dir" == "${RUNNER_TEMP:-/tmp}/zones-benchmark-auth."* ]]; then
        local -a auth_temp_files=()
        shopt -s nullglob
        auth_temp_files=("$secret_dir"/.zone-auth.json.txgen-*.tmp)
        shopt -u nullglob
        rm -f -- "$secret_dir/mnemonic" "$secret_dir/zone-auth.json" "${auth_temp_files[@]}"
        rmdir -- "$secret_dir" 2>/dev/null || true
    fi
    exit "$status"
}
trap cleanup EXIT INT TERM

preflight() {
    local phase="$1"
    local fixture="$2"
    "${preflight_cmd[@]}" \
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
        --check-phase "$phase" \
        --fixture-state "$fixture" \
        --output "$ZONES_BENCH_OUTPUT"
}

run_scenario() {
    local scenario="$1"
    shift
    local -a command=("$txgen_bin" scenario run --scenario "$scenario" "$@")
    if [[ -n "${ZONES_BENCH_CPUSET:-}" ]]; then
        command=(taskset --cpu-list "$ZONES_BENCH_CPUSET" "${command[@]}")
    fi
    "${command[@]}"
}

report_roundtrip_progress() {
    local from_block_hex="$1"
    local event_topic="$2"
    local token_word="$3"
    local amount_word="$4"
    local success_word="$5"
    local logs completed latest_block

    if ! logs="$(cast rpc --rpc-url "$ZONES_BENCH_L1_QUERY_RPC_URL" eth_getLogs \
        "{\"address\":\"$L1_PORTAL_ADDRESS\",\"fromBlock\":\"$from_block_hex\",\"toBlock\":\"latest\",\"topics\":[\"$event_topic\"]}" 2>/dev/null)"; then
        echo "roundtrip progress: unable to query terminal L1 withdrawals" >&2
        return 0
    fi
    if ! completed="$(jq -r \
        --arg token "$token_word" \
        --arg amount "$amount_word" \
        --arg success "$success_word" \
        '[.[] | select(
          .removed != true and
          ((.data[2:66] // "") | ascii_downcase) == $token and
          ((.data[66:130] // "") | ascii_downcase) == $amount and
          ((.data[-64:] // "") | ascii_downcase) == $success
        )] | length' <<<"$logs")"; then
        echo "roundtrip progress: unable to parse terminal L1 withdrawals" >&2
        return 0
    fi
    latest_block="$(cast block-number --rpc-url "$ZONES_BENCH_L1_QUERY_RPC_URL" 2>/dev/null || true)"
    echo "roundtrip progress: $completed/$ZONES_BENCH_COUNT successful terminal withdrawals observed${latest_block:+ through L1 block $latest_block}"
}

monitor_roundtrip_progress() {
    local from_block_hex="$1"
    local event_topic="$2"
    local token_word="$3"
    local amount_word="$4"
    local success_word="$5"
    local sleep_pid=""

    stop_progress_monitor() {
        if [[ -n "$sleep_pid" ]] && kill -0 "$sleep_pid" 2>/dev/null; then
            kill -TERM "$sleep_pid" 2>/dev/null || true
            wait "$sleep_pid" 2>/dev/null || true
        fi
        exit 0
    }
    trap stop_progress_monitor TERM INT

    while true; do
        report_roundtrip_progress \
            "$from_block_hex" "$event_topic" "$token_word" "$amount_word" "$success_word"
        sleep 30 &
        sleep_pid=$!
        wait "$sleep_pid"
        sleep_pid=""
    done
}

# Fresh-state validation also renders the control-account bootstrap workload.
preflight bootstrap empty

run_scenario "$ZONES_BENCH_OUTPUT/bootstrap-scenario.yml" \
    --count 1 \
    --max-in-flight 1 \
    --max-rpc-in-flight 4 \
    --failure-policy fail-fast \
    --step-timeout "$ZONES_BENCH_STEP_TIMEOUT" \
    --seed "$ZONES_BENCH_SEED" \
    --sample-instances 1 \
    --report "$ZONES_BENCH_BOOTSTRAP_REPORT"

jq -e '.started == 1 and .completed == 1 and .failed == 0' \
    "$ZONES_BENCH_BOOTSTRAP_REPORT" >/dev/null ||
    die "sequencer bootstrap scenario did not complete"

# Derive the sequencer key through a mode-0600 file so neither mnemonic nor key enters argv.
secret_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/zones-benchmark-auth.XXXXXX")"
chmod 700 "$secret_dir"
(umask 077; printf '%s\n' "$ZONES_BENCH_MNEMONIC" >"$secret_dir/mnemonic")
sequencer_key="$(cast wallet private-key --mnemonic "$secret_dir/mnemonic" --mnemonic-index 4)"
rm -f -- "$secret_dir/mnemonic"

SEQUENCER_KEY="$sequencer_key" "${fee_cmd[@]}" \
    --l1-rpc-url "$L1_RPC_URL" \
    --portal "$L1_PORTAL_ADDRESS" \
    --token "$ZONES_BENCH_TOKEN" \
    --zone-rpc-url "$ZONE_RPC_URL" \
    --tempo-gas-rate 1 \
    --zone-tx-gas-limit 2000000
unset sequencer_key

# Requery all fees and render measured specs only after the outbox rate is live.
preflight roundtrip ready

zone_id="$(jq -er '.zoneId' "$ZONES_BENCH_OUTPUT/preflight.json")"
zone_chain_id="$(jq -er '.zoneChainId' "$ZONES_BENCH_OUTPUT/preflight.json")"
auth_map="$secret_dir/zone-auth.json"
"$txgen_bin" auth-token-map \
    --spec "$ZONES_BENCH_OUTPUT/zone-roundtrip.yml" \
    --pool users \
    --zone-id "$zone_id" \
    --chain-id "$zone_chain_id" \
    --ttl-secs "$ZONES_BENCH_AUTH_TTL_SECS" \
    --refresh-before-secs "$ZONES_BENCH_AUTH_REFRESH_SECS" \
    --watch \
    --output "$auth_map" >"$ZONES_BENCH_OUTPUT/auth-token-map.log" 2>&1 &
auth_pid=$!

auth_deadline=$((SECONDS + 60))
auth_ready=0
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

progress_start_block="$(cast block-number --rpc-url "$ZONES_BENCH_L1_QUERY_RPC_URL")"
printf -v progress_from_block_hex '0x%x' "$((10#$progress_start_block + 1))"
withdrawal_processed_topic="$(cast sig-event \
    'WithdrawalProcessed(address,bytes32,address,uint128,bool)')"
token_hex="${ZONES_BENCH_TOKEN#0x}"
token_hex="${token_hex#0X}"
printf -v withdrawal_token_word '%064s' "${token_hex,,}"
withdrawal_token_word="${withdrawal_token_word// /0}"
withdrawal_amount_hex="$(cast to-hex "$ZONES_BENCH_WITHDRAWAL_AMOUNT")"
withdrawal_amount_hex="${withdrawal_amount_hex#0x}"
printf -v withdrawal_amount_word '%064s' "${withdrawal_amount_hex,,}"
withdrawal_amount_word="${withdrawal_amount_word// /0}"
printf -v withdrawal_success_word '%064d' 1

monitor_roundtrip_progress \
    "$progress_from_block_hex" \
    "$withdrawal_processed_topic" \
    "$withdrawal_token_word" \
    "$withdrawal_amount_word" \
    "$withdrawal_success_word" &
progress_pid=$!

run_scenario "$ZONES_BENCH_OUTPUT/roundtrip-scenario.yml" \
    --count "$ZONES_BENCH_COUNT" \
    --starts-per-second "$ZONES_BENCH_TPS" \
    --max-in-flight "$ZONES_BENCH_MAX_CONCURRENT" \
    --max-rpc-in-flight "$ZONES_BENCH_MAX_CONCURRENT" \
    --failure-policy continue \
    --step-timeout "$ZONES_BENCH_STEP_TIMEOUT" \
    --seed "$ZONES_BENCH_SEED" \
    --sample-instances "$ZONES_BENCH_SAMPLE_INSTANCES" \
    --report "$ZONES_BENCH_REPORT"

kill -TERM "$progress_pid" 2>/dev/null || true
wait "$progress_pid" 2>/dev/null || true
progress_pid=""
report_roundtrip_progress \
    "$progress_from_block_hex" \
    "$withdrawal_processed_topic" \
    "$withdrawal_token_word" \
    "$withdrawal_amount_word" \
    "$withdrawal_success_word"

jq -e --argjson expected "$ZONES_BENCH_COUNT" \
    '.started == $expected and .completed == $expected and .failed == 0 and .timed_out == 0' \
    "$ZONES_BENCH_REPORT" >/dev/null ||
    die "roundtrip scenario did not complete every requested journey"

echo "roundtrip benchmark report: $ZONES_BENCH_REPORT"
