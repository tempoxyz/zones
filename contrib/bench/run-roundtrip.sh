#!/usr/bin/env bash

set -Eeuo pipefail

bench_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scenario-reporting.sh
source "$bench_dir/scenario-reporting.sh"

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

load_benchmark_mnemonic

for name in \
    L1_RPC_URL ZONE_RPC_URL ZONE_PRIVATE_RPC_URL \
    L1_PORTAL_ADDRESS ZONES_BENCH_TOKEN ZONES_BENCH_SEED \
    ZONES_BENCH_EXPECTED_L1_CHAIN_ID ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID \
    ZONES_BENCH_EXPECTED_ZONE_ID ZONES_BENCH_ACCOUNT_END \
    ZONES_BENCH_CONTROL_ACCOUNT_INDEX ZONES_BENCH_CONTROL_ACCOUNT_END \
    ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX ZONES_BENCH_SEQUENCER_ACCOUNT_END \
    ZONES_BENCH_SEQUENCER_ADDRESS ZONES_BENCH_INBOX ZONES_BENCH_OUTBOX \
    ZONES_BENCH_L1_MAX_FEE_PER_GAS ZONES_BENCH_L1_MAX_PRIORITY_FEE_PER_GAS \
    ZONES_BENCH_ZONE_MAX_FEE_PER_GAS ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS
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
ZONES_BENCH_ACCOUNTS="${ZONES_BENCH_ACCOUNTS:-200}"
ZONES_BENCH_COUNT="${ZONES_BENCH_COUNT:-3000}"
ZONES_BENCH_TPS="${ZONES_BENCH_TPS:-20}"
ZONES_BENCH_MAX_CONCURRENT="${ZONES_BENCH_MAX_CONCURRENT:-12}"
ZONES_BENCH_DEPOSIT_AMOUNT="${ZONES_BENCH_DEPOSIT_AMOUNT:-2000000}"
ZONES_BENCH_ACTIVITY_AMOUNT="${ZONES_BENCH_ACTIVITY_AMOUNT:-1}"
ZONES_BENCH_WITHDRAWAL_AMOUNT="${ZONES_BENCH_WITHDRAWAL_AMOUNT:-1000000}"
ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT="${ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT:-10000000}"
ZONES_BENCH_APPROVAL_GAS_LIMIT="${ZONES_BENCH_APPROVAL_GAS_LIMIT:-2000000}"
ZONES_BENCH_DEPOSIT_GAS_LIMIT="${ZONES_BENCH_DEPOSIT_GAS_LIMIT:-2000000}"
ZONES_BENCH_ACTIVITY_GAS_LIMIT="${ZONES_BENCH_ACTIVITY_GAS_LIMIT:-500000}"
ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT="${ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT:-10000000}"
ZONES_BENCH_OUTPUT="${ZONES_BENCH_OUTPUT:-target/zones-benchmark/roundtrip}"
ZONES_BENCH_REPORT="${ZONES_BENCH_REPORT:-target/zones-benchmark/report-roundtrip.json}"
ZONES_BENCH_BOOTSTRAP_REPORT="${ZONES_BENCH_BOOTSTRAP_REPORT:-target/zones-benchmark/report-bootstrap.json}"
ZONES_BENCH_RENDERED_SCENARIO="${ZONES_BENCH_RENDERED_SCENARIO:-$ZONES_BENCH_OUTPUT/roundtrip-scenario.rendered.yml}"
ZONES_BENCH_STEP_TIMEOUT="${ZONES_BENCH_STEP_TIMEOUT:-10m}"
ZONES_BENCH_RECIPIENT_MODE="${ZONES_BENCH_RECIPIENT_MODE:-existing}"
ZONES_BENCH_AUTH_TTL_SECS="${ZONES_BENCH_AUTH_TTL_SECS:-600}"
ZONES_BENCH_AUTH_REFRESH_SECS="${ZONES_BENCH_AUTH_REFRESH_SECS:-60}"
ZONES_BENCH_SAMPLE_INSTANCES="${ZONES_BENCH_SAMPLE_INSTANCES:-10}"
ZONES_BENCH_PROGRESS_INTERVAL_SECS="${ZONES_BENCH_PROGRESS_INTERVAL_SECS:-10}"
ZONES_BENCH_SETUP_SETTLEMENT_TIMEOUT_SECS="${ZONES_BENCH_SETUP_SETTLEMENT_TIMEOUT_SECS:-120}"

for name in \
    ZONES_BENCH_ACCOUNT_START ZONES_BENCH_ACCOUNT_END \
    ZONES_BENCH_CONTROL_ACCOUNT_INDEX ZONES_BENCH_CONTROL_ACCOUNT_END \
    ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX ZONES_BENCH_SEQUENCER_ACCOUNT_END \
    ZONES_BENCH_L1_MAX_PRIORITY_FEE_PER_GAS ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS \
    ZONES_BENCH_SEED
do
    require_uint "$name"
done
for name in \
    ZONES_BENCH_ACCOUNTS ZONES_BENCH_COUNT ZONES_BENCH_TPS \
    ZONES_BENCH_MAX_CONCURRENT ZONES_BENCH_DEPOSIT_AMOUNT \
    ZONES_BENCH_ACTIVITY_AMOUNT ZONES_BENCH_WITHDRAWAL_AMOUNT \
    ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT ZONES_BENCH_AUTH_TTL_SECS \
    ZONES_BENCH_AUTH_REFRESH_SECS ZONES_BENCH_SAMPLE_INSTANCES \
    ZONES_BENCH_PROGRESS_INTERVAL_SECS ZONES_BENCH_SETUP_SETTLEMENT_TIMEOUT_SECS \
    ZONES_BENCH_APPROVAL_GAS_LIMIT ZONES_BENCH_DEPOSIT_GAS_LIMIT \
    ZONES_BENCH_ACTIVITY_GAS_LIMIT ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT \
    ZONES_BENCH_EXPECTED_L1_CHAIN_ID ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID \
    ZONES_BENCH_EXPECTED_ZONE_ID ZONES_BENCH_L1_MAX_FEE_PER_GAS \
    ZONES_BENCH_ZONE_MAX_FEE_PER_GAS
do
    require_positive_uint "$name"
done

(( 10#$ZONES_BENCH_ACCOUNT_END - 10#$ZONES_BENCH_ACCOUNT_START == 10#$ZONES_BENCH_ACCOUNTS )) ||
    die "topology account range does not match ZONES_BENCH_ACCOUNTS"
(( 10#$ZONES_BENCH_MAX_CONCURRENT <= 10#$ZONES_BENCH_ACCOUNTS )) ||
    die "max-concurrent cannot exceed accounts for an exclusively leased roundtrip pool"
case "$ZONES_BENCH_RECIPIENT_MODE" in
    existing)
        ZONES_BENCH_RECIPIENT_GENERATOR='{ pool: { pool: users, select: random } }'
        ZONES_BENCH_RECIPIENT_POOL=users
        ZONES_BENCH_RECIPIENT_SELECT=random
        ZONES_BENCH_RECIPIENT_ACCOUNT_START="$ZONES_BENCH_ACCOUNT_START"
        ZONES_BENCH_RECIPIENT_ACCOUNT_END="$ZONES_BENCH_ACCOUNT_END"
        ;;
    random)
        ZONES_BENCH_RECIPIENT_GENERATOR=random
        ZONES_BENCH_RECIPIENT_POOL=recipients
        ZONES_BENCH_RECIPIENT_SELECT=lease
        ZONES_BENCH_RECIPIENT_ACCOUNT_START=1000000
        ;;
    *) die "ZONES_BENCH_RECIPIENT_MODE must be existing or random" ;;
esac
(( 10#$ZONES_BENCH_AUTH_REFRESH_SECS < 10#$ZONES_BENCH_AUTH_TTL_SECS )) ||
    die "auth refresh lead time must be below the token TTL"

journeys_per_account=$(((10#$ZONES_BENCH_COUNT + 10#$ZONES_BENCH_ACCOUNTS - 1) / 10#$ZONES_BENCH_ACCOUNTS))
if [[ "$ZONES_BENCH_RECIPIENT_MODE" == random ]]; then
    recipient_slots=$((10#$ZONES_BENCH_ACCOUNTS * journeys_per_account))
    ZONES_BENCH_RECIPIENT_ACCOUNT_END=$((ZONES_BENCH_RECIPIENT_ACCOUNT_START + recipient_slots))
fi

export \
    L1_PORTAL_ADDRESS ZONES_BENCH_TOKEN \
    ZONES_BENCH_EXPECTED_L1_CHAIN_ID ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID \
    ZONES_BENCH_EXPECTED_ZONE_ID ZONES_BENCH_CONTROL_ACCOUNT_INDEX \
    ZONES_BENCH_CONTROL_ACCOUNT_END ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX \
    ZONES_BENCH_SEQUENCER_ACCOUNT_END ZONES_BENCH_SEQUENCER_ADDRESS \
    ZONES_BENCH_INBOX ZONES_BENCH_OUTBOX \
    ZONES_BENCH_L1_MAX_FEE_PER_GAS ZONES_BENCH_L1_MAX_PRIORITY_FEE_PER_GAS \
    ZONES_BENCH_ZONE_MAX_FEE_PER_GAS ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS \
    ZONES_BENCH_ACCOUNT_START ZONES_BENCH_ACCOUNT_END \
    ZONES_BENCH_DEPOSIT_AMOUNT ZONES_BENCH_ACTIVITY_AMOUNT ZONES_BENCH_WITHDRAWAL_AMOUNT \
    ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT ZONES_BENCH_APPROVAL_GAS_LIMIT \
    ZONES_BENCH_DEPOSIT_GAS_LIMIT ZONES_BENCH_ACTIVITY_GAS_LIMIT \
    ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT ZONES_BENCH_RECIPIENT_GENERATOR \
    ZONES_BENCH_RECIPIENT_POOL ZONES_BENCH_RECIPIENT_SELECT \
    ZONES_BENCH_RECIPIENT_ACCOUNT_START ZONES_BENCH_RECIPIENT_ACCOUNT_END

txgen_bin="${TXGEN_TEMPO_BIN:-txgen-tempo}"
command -v "$txgen_bin" >/dev/null || die "txgen-tempo binary not found: $txgen_bin"
command -v cast >/dev/null || die "cast is required"
command -v jq >/dev/null || die "jq is required"

if [[ -n "${ZONES_XTASK_BIN:-}" ]]; then
    [[ -x "$ZONES_XTASK_BIN" ]] || die "ZONES_XTASK_BIN is not executable: $ZONES_XTASK_BIN"
    fee_cmd=("$ZONES_XTASK_BIN" configure-benchmark-fees)
else
    fee_cmd=(cargo run --profile release -p tempo-xtask -- configure-benchmark-fees)
fi

spec_dir="$bench_dir/txgen"
mkdir -p "$ZONES_BENCH_OUTPUT" "$(dirname "$ZONES_BENCH_REPORT")" \
    "$(dirname "$ZONES_BENCH_BOOTSTRAP_REPORT")" \
    "$(dirname "$ZONES_BENCH_RENDERED_SCENARIO")"

auth_pid=""
progress_pid=""
health_pid=""
scenario_pid=""
secret_dir=""
cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if [[ -n "$scenario_pid" ]] && kill -0 "$scenario_pid" 2>/dev/null; then
        kill -TERM "$scenario_pid" 2>/dev/null || true
        wait "$scenario_pid" 2>/dev/null || true
    fi
    if [[ -n "$health_pid" ]] && kill -0 "$health_pid" 2>/dev/null; then
        kill -TERM "$health_pid" 2>/dev/null || true
        wait "$health_pid" 2>/dev/null || true
    fi
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
        rm -f -- "$secret_dir/zone-auth.json" "$secret_dir/auth-token-map.log" "${auth_temp_files[@]}"
        rmdir -- "$secret_dir" 2>/dev/null || true
    fi
    unset ZONES_BENCH_MNEMONIC
    exit "$status"
}
trap cleanup EXIT INT TERM

run_scenario() {
    local scenario="$1"
    shift
    local -a command=("$txgen_bin" scenario run --scenario "$scenario" "$@")
    if [[ -n "${ZONES_BENCH_CPUSET:-}" ]]; then
        command=(taskset --cpu-list "$ZONES_BENCH_CPUSET" "${command[@]}")
    fi
    "${command[@]}"
}

read_l1_uint() {
    local address="$1" signature="$2"
    shift 2
    local value
    value="$(cast call "$address" "$signature" "$@" --rpc-url "$L1_RPC_URL" | awk '{print $1}')"
    [[ "$value" =~ ^[0-9]+$ ]] || die "could not read $signature from $address"
    printf '%s\n' "$value"
}

wait_for_l1_deposit_settlement() {
    local expected processed started_at elapsed progress_bucket=-1

    expected="$(read_l1_uint "$L1_PORTAL_ADDRESS" 'depositCount()(uint64)')"
    started_at="$SECONDS"
    while true; do
        processed="$(read_l1_uint \
            "$L1_PORTAL_ADDRESS" 'lastProcessedDepositNumber()(uint64)')"
        if (( 10#$processed >= 10#$expected )); then
            echo "bootstrap L1 deposit settlement complete: $processed/$expected"
            return
        fi

        elapsed=$((SECONDS - started_at))
        if (( elapsed >= 10#$ZONES_BENCH_SETUP_SETTLEMENT_TIMEOUT_SECS )); then
            die "timed out waiting for bootstrap L1 deposit settlement: $processed/$expected after ${elapsed}s"
        fi
        if (( elapsed / 5 > progress_bucket )); then
            echo "bootstrap L1 deposit settlement: $processed/$expected elapsed=${elapsed}s"
            progress_bucket=$((elapsed / 5))
        fi
        sleep 1
    done
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

query_event_logs() {
    local rpc_url="$1"
    local address="$2"
    local from_block="$3"
    local topic="$4"
    local filter

    filter="$(jq -cn \
        --arg address "$address" \
        --arg fromBlock "$from_block" \
        --arg topic "$topic" \
        '{address: $address, fromBlock: $fromBlock, toBlock: "latest", topics: [$topic]}')"
    cast rpc --rpc-url "$rpc_url" --rpc-timeout 3 eth_getLogs "$filter"
}

count_roundtrip_logs() {
    local kind="$1"

    jq -r \
        --arg kind "$kind" \
        --slurpfile accounts "$progress_account_topics_file" \
        --slurpfile accountWords "$progress_account_words_file" \
        --arg recipientMode "$ZONES_BENCH_RECIPIENT_MODE" \
        --arg token "$progress_token_word" \
        --arg activityAmount "$progress_activity_amount_word" \
        --arg withdrawalAmount "$progress_withdrawal_amount_word" \
        --arg success "$progress_success_word" '
        def member($values; $candidate): any($values[]; . == $candidate);
        def word($log; $index):
            (($log.data // "" | ascii_downcase)
                [(2 + ($index * 64)):(66 + ($index * 64))]);
        [
            .[]
            | select(
                .removed != true and
                (.transactionHash | type) == "string" and
                (.logIndex | type) == "string"
            )
            | . as $log
            | select(
                if $kind == "l1-deposit" then
                    member($accounts[0]; ($log.topics[2] // "" | ascii_downcase)) and
                    word($log; 0) == $token and
                    member($accountWords[0]; word($log; 1))
                elif $kind == "zone-deposit" then
                    member($accounts[0]; ($log.topics[2] // "" | ascii_downcase)) and
                    member($accounts[0]; ($log.topics[3] // "" | ascii_downcase)) and
                    word($log; 0) == $token
                elif $kind == "activity" then
                    member($accounts[0]; ($log.topics[1] // "" | ascii_downcase)) and
                    (
                        $recipientMode == "random" or
                        member($accounts[0]; ($log.topics[2] // "" | ascii_downcase))
                    ) and
                    word($log; 0) == $activityAmount
                elif $kind == "zone-withdrawal" then
                    member($accounts[0]; ($log.topics[2] // "" | ascii_downcase)) and
                    word($log; 0) == $token and
                    (
                        $recipientMode == "random" or
                        member($accountWords[0]; word($log; 1))
                    ) and
                    word($log; 2) == $withdrawalAmount
                elif $kind == "l1-withdrawal" then
                    (
                        $recipientMode == "random" or
                        member($accounts[0]; ($log.topics[1] // "" | ascii_downcase))
                    ) and
                    word($log; 0) == $token and
                    word($log; 1) == $withdrawalAmount and
                    word($log; 2) == $success
                else
                    false
                end
            )
        ]
        | unique_by([.transactionHash, .logIndex])
        | length'
}

report_roundtrip_progress() {
    local l1_deposit_logs zone_deposit_logs activity_logs zone_withdrawal_logs
    local l1_withdrawal_logs l1_deposits zone_deposits activities zone_withdrawals
    local l1_withdrawals l1_block zone_block

    if l1_deposit_logs="$(query_event_logs \
        "$ZONES_BENCH_L1_QUERY_RPC_URL" "$progress_portal" \
        "$progress_l1_from_block" "$progress_deposit_made_topic" 2>/dev/null)"; then
        l1_deposits="$(count_roundtrip_logs l1-deposit \
            <<<"$l1_deposit_logs" 2>/dev/null || printf '?')"
    else
        l1_deposits="?"
    fi
    if zone_deposit_logs="$(query_event_logs \
        "$ZONE_RPC_URL" "$progress_inbox" \
        "$progress_zone_from_block" "$progress_deposit_processed_topic" 2>/dev/null)"; then
        zone_deposits="$(count_roundtrip_logs zone-deposit \
            <<<"$zone_deposit_logs" 2>/dev/null || printf '?')"
    else
        zone_deposits="?"
    fi
    if activity_logs="$(query_event_logs \
        "$ZONE_RPC_URL" "$progress_token" \
        "$progress_zone_from_block" "$progress_transfer_topic" 2>/dev/null)"; then
        activities="$(count_roundtrip_logs activity \
            <<<"$activity_logs" 2>/dev/null || printf '?')"
    else
        activities="?"
    fi
    if zone_withdrawal_logs="$(query_event_logs \
        "$ZONE_RPC_URL" "$progress_outbox" \
        "$progress_zone_from_block" "$progress_withdrawal_requested_topic" 2>/dev/null)"; then
        zone_withdrawals="$(count_roundtrip_logs zone-withdrawal \
            <<<"$zone_withdrawal_logs" 2>/dev/null || printf '?')"
    else
        zone_withdrawals="?"
    fi
    if l1_withdrawal_logs="$(query_event_logs \
        "$ZONES_BENCH_L1_QUERY_RPC_URL" "$progress_portal" \
        "$progress_l1_from_block" "$progress_withdrawal_processed_topic" 2>/dev/null)"; then
        l1_withdrawals="$(count_roundtrip_logs l1-withdrawal \
            <<<"$l1_withdrawal_logs" 2>/dev/null || printf '?')"
    else
        l1_withdrawals="?"
    fi

    l1_block="$(cast block-number --rpc-url "$ZONES_BENCH_L1_QUERY_RPC_URL" \
        --rpc-timeout 3 2>/dev/null || printf '?')"
    zone_block="$(cast block-number --rpc-url "$ZONE_RPC_URL" \
        --rpc-timeout 3 2>/dev/null || printf '?')"
    echo "roundtrip progress: L1 deposits $l1_deposits/$ZONES_BENCH_COUNT | Zone deposits $zone_deposits/$ZONES_BENCH_COUNT | activities $activities/$ZONES_BENCH_COUNT | Zone withdrawals $zone_withdrawals/$ZONES_BENCH_COUNT | L1 withdrawals $l1_withdrawals/$ZONES_BENCH_COUNT (L1 block $l1_block, Zone block $zone_block)"
}

monitor_roundtrip_progress() {
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
        report_roundtrip_progress
        sleep "$ZONES_BENCH_PROGRESS_INTERVAL_SECS" &
        sleep_pid=$!
        wait "$sleep_pid"
        sleep_pid=""
    done
}

strip_ansi() {
    sed -E $'s/\x1B\\[[0-9;]*[[:alpha:]]//g'
}

zone_fatal_log_pattern='mismatched block state root|Error advancing the chain: Invalid payload for block|(^|[^[:alnum:]_])(panic|panicked|fatal)([^[:alnum:]_]|$)'
zone_state_root_fallback_pattern='State root task returned incorrect state root'

report_zone_state_root_fallbacks() {
    local chunk="$1"
    local warning_file="$2"
    local line summary

    while IFS= read -r line; do
        [[ -n "$line" ]] || continue
        grep -Fqx -- "$line" "$warning_file" 2>/dev/null && continue
        printf '%s\n' "$line" >>"$warning_file"
        summary="$(sed -E \
            's/^.*(block_num=[0-9]+).*State root task returned incorrect state root (state_root=[^ ]+ block_state_root=[^ ]+).*$/\1 \2/' \
            <<<"$line")"
        echo "Zone state-root task used synchronous fallback: $summary" >&2
    done < <(grep -Ei "$zone_state_root_fallback_pattern" <<<"$chunk" || true)
}

zone_process_problem() {
    local pid="$1"
    local expected="$2"
    local state actual=""

    if ! kill -0 "$pid" 2>/dev/null; then
        echo "Zone process $pid is no longer running"
        return 0
    fi
    if [[ ! -r "/proc/$pid/stat" || ! -r "/proc/$pid/cmdline" ]]; then
        echo "Zone process $pid is no longer inspectable"
        return 0
    fi
    state="$(sed -E 's/^.*\) ([[:alpha:]]) .*/\1/' "/proc/$pid/stat" 2>/dev/null || true)"
    if [[ "$state" == "Z" ]]; then
        echo "Zone process $pid is a zombie"
        return 0
    fi
    IFS= read -r -d '' actual <"/proc/$pid/cmdline" || true
    if [[ "$actual" != "$expected" ]]; then
        echo "Zone PID $pid command mismatch: expected $expected, found ${actual:-<empty>}"
        return 0
    fi
    return 1
}

fail_zone_health() {
    local scenario="$1"
    local failure_file="$2"
    local reason="$3"
    local evidence="${4:-}"
    local attempt

    {
        echo "Zone health failure: $reason"
        [[ -z "$evidence" ]] || echo "Zone log evidence: $evidence"
    } >"$failure_file"
    cat "$failure_file" >&2
    kill -TERM "$scenario" 2>/dev/null || true
    for attempt in 1 2 3 4 5; do
        kill -0 "$scenario" 2>/dev/null || exit 0
        sleep 1
    done
    kill -KILL "$scenario" 2>/dev/null || true
    exit 0
}

monitor_zone_health() {
    local zone_pid="$1"
    local expected="$2"
    local log_file="$3"
    local offset="$4"
    local scenario="$5"
    local failure_file="$6"
    local warning_file="$7"
    local problem size scan_start chunk match

    trap 'exit 0' TERM INT
    while kill -0 "$scenario" 2>/dev/null; do
        if problem="$(zone_process_problem "$zone_pid" "$expected")"; then
            fail_zone_health "$scenario" "$failure_file" "$problem"
        fi
        if [[ ! -f "$log_file" ]]; then
            fail_zone_health "$scenario" "$failure_file" \
                "Zone log disappeared: $log_file"
        fi
        size="$(stat -c '%s' "$log_file" 2>/dev/null || true)"
        if [[ ! "$size" =~ ^[0-9]+$ ]]; then
            fail_zone_health "$scenario" "$failure_file" \
                "cannot inspect Zone log size: $log_file"
        fi
        if (( size < offset )); then
            offset=0
        fi
        if (( size > offset )); then
            if (( offset > 1024 )); then
                scan_start=$((offset - 1023))
            else
                scan_start=1
            fi
            chunk="$(tail -c "+$scan_start" "$log_file" 2>/dev/null | strip_ansi || true)"
            report_zone_state_root_fallbacks "$chunk" "$warning_file"
            if match="$(grep -Eim1 "$zone_fatal_log_pattern" <<<"$chunk")"; then
                fail_zone_health "$scenario" "$failure_file" \
                    "fatal execution evidence appeared in $log_file" "$match"
            fi
            offset=$size
        fi
        sleep 1
    done
}

prepare_zone_health_monitor() {
    local zone_line existing_match fallback_count problem _ extra

    zone_health_enabled=0
    if [[ -z "${ZONES_BENCH_PID_FILE:-}" && -z "${ZONES_BENCH_TOPOLOGY_DIR:-}" ]]; then
        echo "Zone health supervision unavailable: set ZONES_BENCH_PID_FILE and ZONES_BENCH_TOPOLOGY_DIR" >&2
        return
    fi
    [[ -n "${ZONES_BENCH_PID_FILE:-}" ]] || die \
        "ZONES_BENCH_PID_FILE is required when ZONES_BENCH_TOPOLOGY_DIR is set"
    [[ -n "${ZONES_BENCH_TOPOLOGY_DIR:-}" ]] || die \
        "ZONES_BENCH_TOPOLOGY_DIR is required when ZONES_BENCH_PID_FILE is set"
    [[ -f "$ZONES_BENCH_PID_FILE" ]] || die \
        "Zone PID file does not exist: $ZONES_BENCH_PID_FILE"

    zone_line="$(awk '
        $1 == "zone" { count += 1; line = $0 }
        END { if (count == 1) print line; else exit 1 }
    ' "$ZONES_BENCH_PID_FILE")" || die \
        "Zone PID file must contain exactly one zone entry: $ZONES_BENCH_PID_FILE"
    read -r _ zone_health_zone_pid zone_health_expected extra <<<"$zone_line"
    [[ "$zone_health_zone_pid" =~ ^[1-9][0-9]*$ && -n "$zone_health_expected" && -z "${extra:-}" ]] ||
        die "invalid zone entry in PID file: $ZONES_BENCH_PID_FILE"

    zone_health_log="$ZONES_BENCH_TOPOLOGY_DIR/logs/zone.log"
    [[ -f "$zone_health_log" ]] || die "Zone log does not exist: $zone_health_log"
    if problem="$(zone_process_problem "$zone_health_zone_pid" "$zone_health_expected")"; then
        die "$problem"
    fi
    if existing_match="$(grep -Eim1 "$zone_fatal_log_pattern" "$zone_health_log")"; then
        existing_match="$(strip_ansi <<<"$existing_match")"
        die "Zone log already contains fatal execution evidence: $existing_match"
    fi
    zone_health_log_offset="$(stat -c '%s' "$zone_health_log")"
    zone_health_failure_file="$ZONES_BENCH_OUTPUT/zone-health-failure.log"
    zone_health_warning_file="$ZONES_BENCH_OUTPUT/zone-state-root-fallbacks.log"
    rm -f -- "$zone_health_failure_file"
    grep -Ei "$zone_state_root_fallback_pattern" "$zone_health_log" 2>/dev/null \
        | strip_ansi | sort -u >"$zone_health_warning_file" || true
    if [[ -s "$zone_health_warning_file" ]]; then
        fallback_count="$(wc -l <"$zone_health_warning_file")"
        echo "Zone state-root task already used synchronous fallback $fallback_count time(s) during setup" >&2
    fi
    zone_health_enabled=1
    echo "Zone health supervision active for PID $zone_health_zone_pid ($zone_health_log)"
}

# The bootstrap includes its control-account portal approval and waits for both
# the approval receipt and the exact terminal Zone deposit event.
run_scenario "$spec_dir/bootstrap-scenario.yml" \
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

# Reuse the protected benchmark mnemonic file so the phrase never enters argv or a second file.
secret_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/zones-benchmark-auth.XXXXXX")"
chmod 700 "$secret_dir"
sequencer_key="$(cast wallet private-key \
    --mnemonic "$ZONES_BENCH_MNEMONIC_FILE" \
    --mnemonic-index 4)"

SEQUENCER_KEY="$sequencer_key" "${fee_cmd[@]}" \
    --l1-rpc-url "$L1_RPC_URL" \
    --portal "$L1_PORTAL_ADDRESS" \
    --token "$ZONES_BENCH_TOKEN" \
    --zone-rpc-url "$ZONE_RPC_URL" \
    --tempo-gas-rate 1 \
    --zone-tx-gas-limit 2000000
unset sequencer_key

# The Zone terminal event proves execution. Wait for L1 batch finalization before
# changing the outbox fee and starting user setup.
wait_for_l1_deposit_settlement

auth_map="$secret_dir/zone-auth.json"
"$txgen_bin" auth-token-map \
    --spec "$spec_dir/zone-roundtrip.yml" \
    --pool users \
    --zone-id "$ZONES_BENCH_EXPECTED_ZONE_ID" \
    --chain-id "$ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID" \
    --ttl-secs "$ZONES_BENCH_AUTH_TTL_SECS" \
    --refresh-before-secs "$ZONES_BENCH_AUTH_REFRESH_SECS" \
    --watch \
    --output "$auth_map" >"$secret_dir/auth-token-map.log" 2>&1 &
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

run_approval_scenario portal "$spec_dir/portal-approval-scenario.yml"
run_approval_scenario outbox "$spec_dir/outbox-approval-scenario.yml"

# Run the composed document so txgen reports fragment provenance. The results renderer
# consumes a deterministic flattened copy because it validates concrete scenario steps.
"$txgen_bin" scenario render \
    --scenario "$spec_dir/roundtrip-scenario.yml" \
    --output "$ZONES_BENCH_RENDERED_SCENARIO"
[[ -s "$ZONES_BENCH_RENDERED_SCENARIO" ]] ||
    die "txgen did not render the composed roundtrip scenario"

prepare_zone_health_monitor

progress_l1_start_block="$(cast block-number --rpc-url "$ZONES_BENCH_L1_QUERY_RPC_URL")"
progress_zone_start_block="$(cast block-number --rpc-url "$ZONE_RPC_URL")"
printf -v progress_l1_from_block '0x%x' "$((10#$progress_l1_start_block + 1))"
printf -v progress_zone_from_block '0x%x' "$((10#$progress_zone_start_block + 1))"
progress_deposit_made_topic="$(cast sig-event \
    'DepositMade(bytes32,address,address,address,uint128,uint128,bytes32,address,uint64)')"
progress_deposit_processed_topic="$(cast sig-event \
    'DepositProcessed(bytes32,address,address,address,uint128,bytes32)')"
progress_transfer_topic="$(cast sig-event 'Transfer(address,address,uint256)')"
progress_withdrawal_requested_topic="$(cast sig-event \
    'WithdrawalRequested(uint64,address,address,address,uint128,uint128,bytes32,uint64,uint64,bytes,bytes)')"
progress_withdrawal_processed_topic="$(cast sig-event \
    'WithdrawalProcessed(address,bytes32,address,uint128,bool)')"
progress_portal="$L1_PORTAL_ADDRESS"
progress_outbox="$ZONES_BENCH_OUTBOX"
progress_token="$ZONES_BENCH_TOKEN"
progress_inbox="$ZONES_BENCH_INBOX"
progress_accounts_file="$ZONES_BENCH_OUTPUT/progress-accounts.json"
jq -c 'keys | sort' "$auth_map" >"$progress_accounts_file"
jq -e 'type == "array" and length > 0 and all(.[]; test("^0x[0-9a-fA-F]{40}$"))' \
    "$progress_accounts_file" >/dev/null ||
    die "txgen auth map did not contain valid progress account addresses"
progress_account_topics_file="$ZONES_BENCH_OUTPUT/progress-account-topics.json"
progress_account_words_file="$ZONES_BENCH_OUTPUT/progress-account-words.json"
jq -c '
    [.[]
        | ascii_downcase
        | ltrimstr("0x")
        | "0x000000000000000000000000" + .]
    | unique
' "$progress_accounts_file" >"$progress_account_topics_file"
jq -c '
    [.[]
        | ascii_downcase
        | ltrimstr("0x")
        | "000000000000000000000000" + .]
    | unique
' "$progress_accounts_file" >"$progress_account_words_file"
token_hex="${progress_token#0x}"
token_hex="${token_hex#0X}"
printf -v progress_token_word '%064s' "${token_hex,,}"
progress_token_word="${progress_token_word// /0}"
activity_amount_hex="$(cast to-hex "$ZONES_BENCH_ACTIVITY_AMOUNT")"
activity_amount_hex="${activity_amount_hex#0x}"
printf -v progress_activity_amount_word '%064s' "${activity_amount_hex,,}"
progress_activity_amount_word="${progress_activity_amount_word// /0}"
withdrawal_amount_hex="$(cast to-hex "$ZONES_BENCH_WITHDRAWAL_AMOUNT")"
withdrawal_amount_hex="${withdrawal_amount_hex#0x}"
printf -v progress_withdrawal_amount_word '%064s' "${withdrawal_amount_hex,,}"
progress_withdrawal_amount_word="${progress_withdrawal_amount_word// /0}"
printf -v progress_success_word '%064d' 1

monitor_roundtrip_progress &
progress_pid=$!

scenario_report_args=()
build_scenario_report_args scenario_report_args "$ZONES_BENCH_REPORT"
scenario_command=(
    "$txgen_bin" scenario run
    --scenario "$spec_dir/roundtrip-scenario.yml"
    --count "$ZONES_BENCH_COUNT"
    --starts-per-second "$ZONES_BENCH_TPS"
    --max-in-flight "$ZONES_BENCH_MAX_CONCURRENT"
    --max-rpc-in-flight "$ZONES_BENCH_MAX_CONCURRENT"
    --failure-policy continue
    --step-timeout "$ZONES_BENCH_STEP_TIMEOUT"
    --seed "$ZONES_BENCH_SEED"
    --sample-instances "$ZONES_BENCH_SAMPLE_INSTANCES"
    "${scenario_report_args[@]}"
)
if [[ -n "${ZONES_BENCH_CPUSET:-}" ]]; then
    scenario_command=(taskset --cpu-list "$ZONES_BENCH_CPUSET" "${scenario_command[@]}")
fi
"${scenario_command[@]}" &
scenario_pid=$!

if (( zone_health_enabled == 1 )); then
    monitor_zone_health \
        "$zone_health_zone_pid" \
        "$zone_health_expected" \
        "$zone_health_log" \
        "$zone_health_log_offset" \
        "$scenario_pid" \
        "$zone_health_failure_file" \
        "$zone_health_warning_file" &
    health_pid=$!
fi

scenario_status=0
wait "$scenario_pid" || scenario_status=$?
scenario_pid=""

if [[ -n "$health_pid" ]]; then
    kill -TERM "$health_pid" 2>/dev/null || true
    wait "$health_pid" 2>/dev/null || true
    health_pid=""
fi

kill -TERM "$progress_pid" 2>/dev/null || true
wait "$progress_pid" 2>/dev/null || true
progress_pid=""

if (( zone_health_enabled == 1 )) && [[ -s "$zone_health_failure_file" ]]; then
    die "roundtrip stopped because the Zone became unhealthy; see $zone_health_failure_file"
fi
if (( zone_health_enabled == 1 )) && [[ -s "$zone_health_warning_file" ]]; then
    echo "Zone state-root synchronous fallbacks observed: $(wc -l <"$zone_health_warning_file")"
fi
report_roundtrip_progress
(( scenario_status == 0 )) || die "roundtrip scenario failed with status $scenario_status"

jq -e --argjson expected "$ZONES_BENCH_COUNT" \
    '.started == $expected and .completed == $expected and .failed == 0 and .timed_out == 0' \
    "$ZONES_BENCH_REPORT" >/dev/null ||
    die "roundtrip scenario did not complete every requested journey"

echo "roundtrip benchmark report: $ZONES_BENCH_REPORT"
