#!/usr/bin/env bash

# Run the complete private-Zone journey. Provisioning, fixture deployment,
# admission seeding, approvals, and private-RPC authentication are deliberately
# outside the scenario measurement.
set -Eeuo pipefail

bench_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scenario-reporting.sh
source "$bench_dir/scenario-reporting.sh"

die() { echo "error: $*" >&2; exit 1; }
need() { [[ -n "${!1:-}" ]] || die "$1 must be set"; }
uint() { [[ "${!1:-}" =~ ^[0-9]+$ ]] || die "$1 must be an unsigned integer"; }
positive_rate() {
    local value="${!1:-}"
    if ! [[ "$value" =~ ^([0-9]+([.][0-9]+)?|[.][0-9]+)$ ]] ||
        ! awk -v value="$value" 'BEGIN { exit !(value > 0 && value <= 999999999) }'; then
        die "$1 must be a positive decimal no greater than 999999999"
    fi
}

load_benchmark_mnemonic() {
    local mode

    need ZONES_BENCH_MNEMONIC_FILE
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

# Accept the legacy topology names from older cached/provisioned environments,
# but use the canonical Earn v1 contract identities throughout this runtime.
ZONES_BENCH_EARN_ROUTER="${ZONES_BENCH_EARN_ROUTER:-${ZONES_BENCH_GATEWAY:-}}"
ZONES_BENCH_EARN_VAULT="${ZONES_BENCH_EARN_VAULT:-${ZONES_BENCH_VAULT_ADAPTER:-}}"
ZONES_BENCH_EARN_CONTRIBUTION_CONTROLLER="${ZONES_BENCH_EARN_CONTRIBUTION_CONTROLLER:-${ZONES_BENCH_REWARDS:-}}"

for name in L1_RPC_URL L1_WS_RPC_URL ZONE_RPC_URL ZONE_WS_RPC_URL ZONE_PRIVATE_RPC_URL \
    L1_PORTAL_ADDRESS ZONES_BENCH_TOKEN ZONES_BENCH_DLUSD ZONES_BENCH_PATHUSD ZONES_BENCH_EARN_TOKEN \
    ZONES_BENCH_EARN_ROUTER ZONES_BENCH_BRIDGE_WALLET ZONES_BENCH_VAULT ZONES_BENCH_ENGINE \
    ZONES_BENCH_EARN_VAULT ZONES_BENCH_EARN_CONTRIBUTION_CONTROLLER ZONES_BENCH_SEED \
    ZONES_BENCH_EXPECTED_L1_CHAIN_ID ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID \
    ZONES_BENCH_EXPECTED_ZONE_ID ZONES_BENCH_SEQUENCER_ADDRESS
do need "$name"; done

ZONES_BENCH_CONTROL_ACCOUNT_INDEX="${ZONES_BENCH_CONTROL_ACCOUNT_INDEX:-0}"
ZONES_BENCH_ACCOUNT_START="${ZONES_BENCH_ACCOUNT_START:-16}"
ZONES_BENCH_ACCOUNTS="${ZONES_BENCH_ACCOUNTS:-100}"
ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX="${ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX:-4}"
ZONES_BENCH_COUNT="${ZONES_BENCH_COUNT:-100}"
ZONES_BENCH_TPS="${ZONES_BENCH_TPS:-20}"
ZONES_BENCH_MAX_CONCURRENT="${ZONES_BENCH_MAX_CONCURRENT:-12}"
ZONES_BENCH_DEPOSIT_AMOUNT="${ZONES_BENCH_DEPOSIT_AMOUNT:-2000000}"
ZONES_BENCH_ACTIVITY_AMOUNT="${ZONES_BENCH_ACTIVITY_AMOUNT:-1}"
ZONES_BENCH_WITHDRAWAL_AMOUNT="${ZONES_BENCH_WITHDRAWAL_AMOUNT:-1000000}"
ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT="${ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT:-10000000}"
# The canonical Zone boundary e2e uses the full callback allowance: a gateway
# callback can include a swap, vault action, and encrypted return deposit.
ZONES_BENCH_CALLBACK_GAS_LIMIT="${ZONES_BENCH_CALLBACK_GAS_LIMIT:-10000000}"
ZONES_BENCH_OUTPUT="${ZONES_BENCH_OUTPUT:-target/zones-benchmark/neobank-e2e}"
ZONES_BENCH_REPORT="${ZONES_BENCH_REPORT:-target/zones-benchmark/report-neobank-e2e.json}"
ZONES_BENCH_RENDERED_SCENARIO="${ZONES_BENCH_RENDERED_SCENARIO:-$ZONES_BENCH_OUTPUT/private-flow-scenario.rendered.yml}"
ZONES_BENCH_WITHDRAWAL_BLOCKS_REPORT="${ZONES_BENCH_WITHDRAWAL_BLOCKS_REPORT:-target/zones-benchmark/withdrawal-blocks.json}"
ZONES_BENCH_WITHDRAWAL_BLOCKS_SUMMARY="${ZONES_BENCH_WITHDRAWAL_BLOCKS_SUMMARY:-target/zones-benchmark/withdrawal-blocks.md}"
l1_measurement_rpc="${ZONES_BENCH_L1_QUERY_RPC_URL:-$L1_RPC_URL}"
ZONES_BENCH_AUTH_TTL_SECS="${ZONES_BENCH_AUTH_TTL_SECS:-600}"
ZONES_BENCH_AUTH_REFRESH_SECS="${ZONES_BENCH_AUTH_REFRESH_SECS:-60}"
ZONES_BENCH_STEP_TIMEOUT="${ZONES_BENCH_STEP_TIMEOUT:-10m}"
ZONES_BENCH_SAMPLE_INSTANCES="${ZONES_BENCH_SAMPLE_INSTANCES:-10}"
ZONES_BENCH_RUN_ID="${ZONES_BENCH_RUN_ID:-local}"
ZONES_BENCH_RECIPIENT_MODE="${ZONES_BENCH_RECIPIENT_MODE:-existing}"
ZONES_BENCH_NEOBANK_PRESET="${ZONES_BENCH_NEOBANK_PRESET:-full-journey}"
ZONES_BENCH_SWAP_MECHANISM="${ZONES_BENCH_SWAP_MECHANISM:-direct-swap}"
ZONES_BENCH_L1_QUERY_RPC_URL="${ZONES_BENCH_L1_QUERY_RPC_URL:-$L1_RPC_URL}"
ZONES_BENCH_ACCOUNT_END="${ZONES_BENCH_ACCOUNT_END:-$((10#$ZONES_BENCH_ACCOUNT_START + 10#$ZONES_BENCH_ACCOUNTS))}"
ZONES_BENCH_CONTROL_ACCOUNT_END="${ZONES_BENCH_CONTROL_ACCOUNT_END:-$((10#$ZONES_BENCH_CONTROL_ACCOUNT_INDEX + 1))}"
ZONES_BENCH_SEQUENCER_ACCOUNT_END="${ZONES_BENCH_SEQUENCER_ACCOUNT_END:-$((10#$ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX + 1))}"
ZONES_BENCH_INBOX="${ZONES_BENCH_INBOX:-0x1c00000000000000000000000000000000000001}"
ZONES_BENCH_OUTBOX="${ZONES_BENCH_OUTBOX:-0x1c00000000000000000000000000000000000002}"
ZONES_BENCH_L1_MAX_FEE_PER_GAS="${ZONES_BENCH_L1_MAX_FEE_PER_GAS:-12000000000}"
ZONES_BENCH_L1_MAX_PRIORITY_FEE_PER_GAS=0
ZONES_BENCH_ZONE_MAX_FEE_PER_GAS="${ZONES_BENCH_ZONE_MAX_FEE_PER_GAS:-10000000000}"
ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS=0
ZONES_BENCH_DEPOSIT_GAS_LIMIT="${ZONES_BENCH_DEPOSIT_GAS_LIMIT:-2000000}"
ZONES_BENCH_ACTIVITY_GAS_LIMIT="${ZONES_BENCH_ACTIVITY_GAS_LIMIT:-500000}"
ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT="${ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT:-10000000}"
ZONES_BENCH_APPROVAL_GAS_LIMIT="${ZONES_BENCH_APPROVAL_GAS_LIMIT:-2000000}"
ZONES_BENCH_ADMISSION_SEED_AMOUNT="${ZONES_BENCH_ADMISSION_SEED_AMOUNT:-1}"
ZONES_BENCH_RECIPIENT_ACCOUNT_START="${ZONES_BENCH_RECIPIENT_ACCOUNT_START:-$ZONES_BENCH_ACCOUNT_START}"
ZONES_BENCH_RECIPIENT_ACCOUNT_END="${ZONES_BENCH_RECIPIENT_ACCOUNT_END:-$ZONES_BENCH_ACCOUNT_END}"
if [[ -z "${ZONES_BENCH_RECIPIENT_GENERATOR:-}" ]]; then
    ZONES_BENCH_RECIPIENT_GENERATOR='{ pool: { pool: users, select: random } }'
fi
withdrawals_per_journey=0
earn_deposits_per_journey=0
earn_redeems_per_journey=0
offramps_per_journey=0
earn_deposit_callback_successes_per_journey=0
earn_redeem_callback_successes_per_journey=0
case "$ZONES_BENCH_RECIPIENT_MODE" in
    existing) ZONES_BENCH_PRIVATE_TRANSFER_RECIPIENT='{ var: recipient.address }' ;;
    random) ZONES_BENCH_PRIVATE_TRANSFER_RECIPIENT=random ;;
    *) die "ZONES_BENCH_RECIPIENT_MODE must be existing or random" ;;
esac
case "$ZONES_BENCH_NEOBANK_PRESET" in
    direct-lifecycle)
        scenario_file=direct-lifecycle-scenario.yml
        base_token_label=pathusd
        expected_base_token="$ZONES_BENCH_PATHUSD"
        leases_per_journey=1
        withdrawals_per_journey=2
        earn_deposits_per_journey=1
        earn_redeems_per_journey=1
        earn_deposit_callback_successes_per_journey=1
        earn_redeem_callback_successes_per_journey=1
        ;;
    encrypted-deposit)
        scenario_file=encrypted-deposit-scenario.yml
        base_token_label=dlusd
        expected_base_token="$ZONES_BENCH_DLUSD"
        leases_per_journey=1
        withdrawals_per_journey=0
        ;;
    third-party-recipient)
        scenario_file=third-party-recipient-scenario.yml
        base_token_label=pathusd
        expected_base_token="$ZONES_BENCH_PATHUSD"
        leases_per_journey=2
        withdrawals_per_journey=2
        earn_deposits_per_journey=1
        earn_redeems_per_journey=1
        earn_deposit_callback_successes_per_journey=1
        earn_redeem_callback_successes_per_journey=1
        ;;
    full-journey)
        scenario_file=private-flow-scenario.yml
        base_token_label=dlusd
        expected_base_token="$ZONES_BENCH_DLUSD"
        leases_per_journey=1
        withdrawals_per_journey=3
        earn_deposits_per_journey=1
        earn_redeems_per_journey=1
        offramps_per_journey=1
        earn_deposit_callback_successes_per_journey=1
        earn_redeem_callback_successes_per_journey=1
        ;;
    private-withdrawal)
        scenario_file=private-withdrawal-scenario.yml
        base_token_label=dlusd
        expected_base_token="$ZONES_BENCH_DLUSD"
        leases_per_journey=1
        withdrawals_per_journey=1
        earn_redeems_per_journey=1
        earn_redeem_callback_successes_per_journey=1
        ;;
    rewards-redemption)
        scenario_file=rewards-redemption-scenario.yml
        base_token_label=pathusd
        expected_base_token="$ZONES_BENCH_PATHUSD"
        leases_per_journey=1
        withdrawals_per_journey=2
        earn_redeems_per_journey=2
        earn_redeem_callback_successes_per_journey=2
        ;;
    slippage-bounce)
        scenario_file=slippage-bounce-scenario.yml
        base_token_label=dlusd
        expected_base_token="$ZONES_BENCH_DLUSD"
        leases_per_journey=1
        withdrawals_per_journey=1
        earn_deposits_per_journey=1
        ;;
    swapped-lifecycle)
        scenario_file=swapped-lifecycle-scenario.yml
        base_token_label=dlusd
        expected_base_token="$ZONES_BENCH_DLUSD"
        leases_per_journey=1
        withdrawals_per_journey=2
        earn_deposits_per_journey=1
        earn_redeems_per_journey=1
        earn_deposit_callback_successes_per_journey=1
        earn_redeem_callback_successes_per_journey=1
        ;;
    swapped-redemption)
        scenario_file=swapped-redemption-scenario.yml
        base_token_label=dlusd
        expected_base_token="$ZONES_BENCH_DLUSD"
        leases_per_journey=1
        withdrawals_per_journey=1
        earn_redeems_per_journey=1
        earn_redeem_callback_successes_per_journey=1
        ;;
    *) die "unsupported neobank preset: $ZONES_BENCH_NEOBANK_PRESET" ;;
esac
case "$ZONES_BENCH_SWAP_MECHANISM" in
    direct-swap|simple|stablecoin-dex) ;;
    *) die "ZONES_BENCH_SWAP_MECHANISM must be direct-swap, simple, or stablecoin-dex" ;;
esac
[[ "${ZONES_BENCH_TOKEN,,}" == "${expected_base_token,,}" ]] ||
    die "ZONES_BENCH_TOKEN must match the $base_token_label token for $ZONES_BENCH_NEOBANK_PRESET"
for name in ZONES_BENCH_CONTROL_ACCOUNT_INDEX ZONES_BENCH_ACCOUNT_START ZONES_BENCH_ACCOUNTS ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX ZONES_BENCH_COUNT \
    ZONES_BENCH_MAX_CONCURRENT ZONES_BENCH_DEPOSIT_AMOUNT ZONES_BENCH_ACTIVITY_AMOUNT \
    ZONES_BENCH_WITHDRAWAL_AMOUNT ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT \
    ZONES_BENCH_CALLBACK_GAS_LIMIT ZONES_BENCH_SAMPLE_INSTANCES ZONES_BENCH_SEED \
    ZONES_BENCH_ACCOUNT_END ZONES_BENCH_CONTROL_ACCOUNT_END \
    ZONES_BENCH_SEQUENCER_ACCOUNT_END ZONES_BENCH_EXPECTED_L1_CHAIN_ID \
    ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID ZONES_BENCH_EXPECTED_ZONE_ID \
    ZONES_BENCH_L1_MAX_FEE_PER_GAS ZONES_BENCH_ZONE_MAX_FEE_PER_GAS \
    ZONES_BENCH_DEPOSIT_GAS_LIMIT ZONES_BENCH_ACTIVITY_GAS_LIMIT \
    ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT ZONES_BENCH_APPROVAL_GAS_LIMIT \
    ZONES_BENCH_ADMISSION_SEED_AMOUNT ZONES_BENCH_RECIPIENT_ACCOUNT_START \
    ZONES_BENCH_RECIPIENT_ACCOUNT_END
do uint "$name"; done
positive_rate ZONES_BENCH_TPS
(( 10#$ZONES_BENCH_ACCOUNTS > 0 && 10#$ZONES_BENCH_COUNT > 0 )) || die "accounts and count must be positive"
(( 10#$ZONES_BENCH_MAX_CONCURRENT > 0 )) || die "max concurrency must be positive"
(( 10#$ZONES_BENCH_SAMPLE_INSTANCES > 0 )) ||
    die "sample instances must be positive"
sample_instances="$ZONES_BENCH_SAMPLE_INSTANCES"
if (( 10#$ZONES_BENCH_COUNT < 10#$sample_instances )); then
    sample_instances="$ZONES_BENCH_COUNT"
fi
[[ "$ZONES_BENCH_CONTROL_ACCOUNT_INDEX" == 0 ]] ||
    die "this topology fixes the neobank control account at mnemonic index 0"
[[ "$ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX" == 4 ]] ||
    die "this topology fixes the approval sponsor at mnemonic index 4"
required_accounts=$((10#$ZONES_BENCH_MAX_CONCURRENT * leases_per_journey))
(( required_accounts <= 10#$ZONES_BENCH_ACCOUNTS )) ||
    die "$ZONES_BENCH_NEOBANK_PRESET requires at least $required_accounts accounts for max-concurrent=$ZONES_BENCH_MAX_CONCURRENT"

# Reward sizing uses arbitrary-precision arithmetic so configured uint128
# amounts cannot silently wrap in the shell.
reward_onramp_per_account="$ZONES_BENCH_DEPOSIT_AMOUNT"
reward_position_per_account="$ZONES_BENCH_WITHDRAWAL_AMOUNT"
reward_total_position=0
reward_fund_amount=1
reward_first_redeem_amount=1
reward_second_redeem_amount=1
reward_expected_remaining=0
if [[ "$ZONES_BENCH_NEOBANK_PRESET" == "rewards-redemption" ]]; then
    command -v python3 >/dev/null || die "missing python3"
    reward_sizing="$(python3 - \
        "$ZONES_BENCH_ACCOUNTS" "$ZONES_BENCH_COUNT" \
        "$ZONES_BENCH_DEPOSIT_AMOUNT" "$ZONES_BENCH_WITHDRAWAL_AMOUNT" <<'PY'
import sys

a, j, d, w = map(int, sys.argv[1:])
u128_max = 2**128 - 1
u256_max = 2**256 - 1
if min(a, j, d, w) <= 0:
    raise SystemExit("rewards-redemption sizing values must be positive")
if w <= 1:
    raise SystemExit("rewards-redemption requires withdrawal-amount greater than 1")
if d <= w:
    raise SystemExit(
        "rewards-redemption requires deposit-amount greater than withdrawal-amount for Zone fees"
    )
n = (j + a - 1) // a
position = n * w
onramp = n * d
total = a * position
reward = total // 10
first = w // 2
second = w - first
redeemed = j * w
remaining = total - redeemed
if max(onramp, position, first, second) > u128_max:
    raise SystemExit("reward scenario uint128 call amount overflow")
if max(total, reward, redeemed, remaining) > u256_max:
    raise SystemExit("reward scenario uint256 accounting overflow")
if reward <= 0 or first <= 0 or second <= 0 or remaining < 0:
    raise SystemExit("invalid reward scenario sizing result")
print(onramp, position, total, reward, first, second, remaining, sep="\n")
PY
)"
    mapfile -t reward_values <<<"$reward_sizing"
    (( ${#reward_values[@]} == 7 )) || die "could not compute reward scenario sizing"
    reward_onramp_per_account="${reward_values[0]}"
    reward_position_per_account="${reward_values[1]}"
    reward_total_position="${reward_values[2]}"
    reward_fund_amount="${reward_values[3]}"
    reward_first_redeem_amount="${reward_values[4]}"
    reward_second_redeem_amount="${reward_values[5]}"
    reward_expected_remaining="${reward_values[6]}"
fi

swapped_redemption_onramp_per_account=1
swapped_redemption_position_per_account=1
swapped_redemption_total_position=0
swapped_redemption_expected_remaining=0
if [[ "$ZONES_BENCH_NEOBANK_PRESET" == "private-withdrawal" ||
      "$ZONES_BENCH_NEOBANK_PRESET" == "swapped-redemption" ]]; then
    swapped_redemption_sizing="$(python3 - \
        "$ZONES_BENCH_ACCOUNTS" "$ZONES_BENCH_COUNT" \
        "$ZONES_BENCH_DEPOSIT_AMOUNT" "$ZONES_BENCH_WITHDRAWAL_AMOUNT" <<'PY'
import sys

accounts, journeys, deposit, withdrawal = map(int, sys.argv[1:])
u128_max = 2**128 - 1
u256_max = 2**256 - 1
if min(accounts, journeys, deposit, withdrawal) <= 0:
    raise SystemExit("swapped-redemption sizing values must be positive")
if deposit <= withdrawal:
    raise SystemExit(
        "swapped-redemption requires deposit-amount greater than withdrawal-amount for Zone fees"
    )
journeys_per_account = (journeys + accounts - 1) // accounts
onramp = journeys_per_account * deposit
position = journeys_per_account * withdrawal
total = accounts * position
redeemed = journeys * withdrawal
remaining = total - redeemed
if max(onramp, position, withdrawal) > u128_max:
    raise SystemExit("swapped-redemption uint128 call amount overflow")
if max(total, redeemed, remaining) > u256_max:
    raise SystemExit("swapped-redemption uint256 accounting overflow")
if remaining < 0:
    raise SystemExit("invalid swapped-redemption sizing result")
print(onramp, position, total, remaining, sep="\n")
PY
)"
    mapfile -t swapped_redemption_values <<<"$swapped_redemption_sizing"
    (( ${#swapped_redemption_values[@]} == 4 )) ||
        die "could not compute swapped-redemption sizing"
    swapped_redemption_onramp_per_account="${swapped_redemption_values[0]}"
    swapped_redemption_position_per_account="${swapped_redemption_values[1]}"
    swapped_redemption_total_position="${swapped_redemption_values[2]}"
    swapped_redemption_expected_remaining="${swapped_redemption_values[3]}"
fi

ZONES_BENCH_REWARD_ONRAMP_PER_ACCOUNT="$reward_onramp_per_account"
ZONES_BENCH_REWARD_POSITION_PER_ACCOUNT="$reward_position_per_account"
ZONES_BENCH_REWARD_FUND_AMOUNT="$reward_fund_amount"
ZONES_BENCH_REWARD_MAX_EARN_SHARE_SUPPLY="$reward_total_position"
ZONES_BENCH_REWARD_FIRST_REDEEM_AMOUNT="$reward_first_redeem_amount"
ZONES_BENCH_REWARD_SECOND_REDEEM_AMOUNT="$reward_second_redeem_amount"
ZONES_BENCH_SWAPPED_REDEMPTION_ONRAMP_PER_ACCOUNT="$swapped_redemption_onramp_per_account"
ZONES_BENCH_SWAPPED_REDEMPTION_POSITION_PER_ACCOUNT="$swapped_redemption_position_per_account"
export L1_RPC_URL L1_WS_RPC_URL ZONE_RPC_URL ZONE_WS_RPC_URL ZONE_PRIVATE_RPC_URL
export L1_PORTAL_ADDRESS ZONES_BENCH_L1_QUERY_RPC_URL ZONES_BENCH_MNEMONIC
export ZONES_BENCH_ACCOUNT_START ZONES_BENCH_ACCOUNT_END ZONES_BENCH_ACCOUNTS
export ZONES_BENCH_CONTROL_ACCOUNT_INDEX ZONES_BENCH_CONTROL_ACCOUNT_END
export ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX ZONES_BENCH_SEQUENCER_ACCOUNT_END
export ZONES_BENCH_SEQUENCER_ADDRESS
export ZONES_BENCH_EXPECTED_L1_CHAIN_ID ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID
export ZONES_BENCH_EXPECTED_ZONE_ID ZONES_BENCH_INBOX ZONES_BENCH_OUTBOX
export ZONES_BENCH_L1_MAX_FEE_PER_GAS ZONES_BENCH_L1_MAX_PRIORITY_FEE_PER_GAS
export ZONES_BENCH_ZONE_MAX_FEE_PER_GAS ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS
export ZONES_BENCH_DEPOSIT_GAS_LIMIT ZONES_BENCH_ACTIVITY_GAS_LIMIT
export ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT ZONES_BENCH_APPROVAL_GAS_LIMIT
export ZONES_BENCH_ADMISSION_SEED_AMOUNT
export ZONES_BENCH_RECIPIENT_ACCOUNT_START ZONES_BENCH_RECIPIENT_ACCOUNT_END
export ZONES_BENCH_RECIPIENT_GENERATOR
export ZONES_BENCH_TOKEN ZONES_BENCH_DLUSD ZONES_BENCH_PATHUSD ZONES_BENCH_EARN_TOKEN
export ZONES_BENCH_EARN_ROUTER ZONES_BENCH_EARN_VAULT
export ZONES_BENCH_EARN_CONTRIBUTION_CONTROLLER ZONES_BENCH_BRIDGE_WALLET
export ZONES_BENCH_DEPOSIT_AMOUNT ZONES_BENCH_ACTIVITY_AMOUNT
export ZONES_BENCH_WITHDRAWAL_AMOUNT ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT
export ZONES_BENCH_CALLBACK_GAS_LIMIT ZONES_BENCH_PRIVATE_TRANSFER_RECIPIENT
export ZONES_BENCH_REWARD_ONRAMP_PER_ACCOUNT ZONES_BENCH_REWARD_POSITION_PER_ACCOUNT
export ZONES_BENCH_REWARD_FUND_AMOUNT ZONES_BENCH_REWARD_MAX_EARN_SHARE_SUPPLY
export ZONES_BENCH_REWARD_FIRST_REDEEM_AMOUNT ZONES_BENCH_REWARD_SECOND_REDEEM_AMOUNT
export ZONES_BENCH_SWAPPED_REDEMPTION_ONRAMP_PER_ACCOUNT
export ZONES_BENCH_SWAPPED_REDEMPTION_POSITION_PER_ACCOUNT

stage_start() { echo "neobank stage=start run_id=$ZONES_BENCH_RUN_ID preset=$ZONES_BENCH_NEOBANK_PRESET stage=$1"; }
stage_end() { echo "neobank stage=end run_id=$ZONES_BENCH_RUN_ID preset=$ZONES_BENCH_NEOBANK_PRESET stage=$1"; }

read_l1_uint() {
    local address="$1" signature="$2"
    shift 2
    local value
    value="$(cast call "$address" "$signature" "$@" --rpc-url "$L1_RPC_URL" | awk '{print $1}')"
    [[ "$value" =~ ^[0-9]+$ ]] || die "could not read $signature from $address"
    printf '%s\n' "$value"
}

assert_scenario_report() {
    local report="$1" expected="$2" label="$3"
    jq -e --argjson expected "$expected" '
        .started == $expected and
        .completed == $expected and
        .failed == 0 and
        .timed_out == 0
    ' "$report" >/dev/null || die "$label did not complete successfully"
}

# Query private balances with each account's token entirely in process memory.
# Authorization tokens never appear in argv, stdout, or an uploaded artifact.
verify_reward_zone_balances() {
    local mode="$1" expected_total="$2" expected_unit="$3"
    local maximum_unit="${4:-$expected_unit}"
    python3 - \
        "$ZONE_PRIVATE_RPC_URL" "$ZONES_BENCH_ZONE_AUTH_MAP" \
        "$ZONES_BENCH_OUTPUT/accounts.json" "$ZONES_BENCH_EARN_TOKEN" \
        "$ZONES_BENCH_ACCOUNTS" "$mode" "$expected_total" "$expected_unit" \
        "$maximum_unit" <<'PY'
import json
import sys
import urllib.error
import urllib.request

rpc_url, auth_path, accounts_path, token, expected_accounts, mode, expected_total, expected_unit, maximum_unit = sys.argv[1:]
expected_accounts = int(expected_accounts)
expected_total = int(expected_total)
expected_unit = int(expected_unit)
maximum_unit = int(maximum_unit)
with open(auth_path, encoding="utf-8") as handle:
    auth = json.load(handle)
with open(accounts_path, encoding="utf-8") as handle:
    accounts = json.load(handle)
if len(accounts) != expected_accounts:
    raise SystemExit(
        f"account list contains {len(accounts)} accounts, expected {expected_accounts}"
    )

balances = []
for request_id, address in enumerate(accounts, 1):
    authorization = auth.get(address.lower())
    if not authorization:
        raise SystemExit(f"authorization map has no entry for benchmark account {address}")
    calldata = "0x70a08231" + address[2:].lower().rjust(64, "0")
    payload = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "eth_call",
            "params": [{"from": address, "to": token, "data": calldata}, "latest"],
        }
    ).encode()
    request = urllib.request.Request(
        rpc_url,
        data=payload,
        headers={
            "Content-Type": "application/json",
            "X-Authorization-Token": authorization,
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            result = json.load(response)
    except (OSError, urllib.error.HTTPError) as error:
        raise SystemExit(f"private Zone balance query failed for {address}: {error}") from None
    if "error" in result:
        raise SystemExit(f"private Zone balance query failed for {address}: {result['error']}")
    balances.append(int(result["result"], 16))

if mode == "seeded":
    bad = next((balance for balance in balances if balance != expected_unit), None)
    if bad is not None:
        raise SystemExit(
            f"reward position balance {bad} does not equal expected {expected_unit}"
        )
elif mode == "remaining":
    bad = next((balance for balance in balances if balance not in (0, expected_unit)), None)
    if bad is not None:
        raise SystemExit(
            f"terminal reward balance {bad} is neither zero nor {expected_unit}"
        )
elif mode == "distributed_remaining":
    bad = next(
        (
            balance
            for balance in balances
            if balance > maximum_unit or balance % expected_unit != 0
        ),
        None,
    )
    if bad is not None:
        raise SystemExit(
            f"terminal EarnToken balance {bad} is not a valid multiple of "
            f"{expected_unit} within the per-account maximum {maximum_unit}"
        )
else:
    raise SystemExit(f"unknown reward balance validation mode: {mode}")
observed_total = sum(balances)
if observed_total != expected_total:
    raise SystemExit(
        f"aggregate Zone EarnToken balance {observed_total} does not equal expected {expected_total}"
    )
print(f"verified {len(balances)} private Zone EarnToken balances; aggregate={observed_total}")
PY
}

txgen_bin="${TXGEN_TEMPO_BIN:-txgen-tempo}"
for command in "$txgen_bin" awk cast jq python3; do
    command -v "$command" >/dev/null || die "missing $command"
done

neobank_specs="$bench_dir/neobank"
generic_specs="$bench_dir/txgen"
scenario_path="$neobank_specs/$scenario_file"
bootstrap_scenario="$generic_specs/bootstrap-scenario.yml"
portal_approval_scenario="$neobank_specs/l1-portal-approval-scenario.yml"
zone_approval_scenario="$neobank_specs/zone-outbox-approvals-scenario.yml"
admission_seed_scenario="$neobank_specs/admission-seed-scenario.yml"
for file in "$scenario_path" "$bootstrap_scenario" "$portal_approval_scenario" \
    "$zone_approval_scenario" "$admission_seed_scenario"
do
    [[ -f "$file" ]] || die "missing txgen scenario $file"
done

mkdir -p "$ZONES_BENCH_OUTPUT" "$(dirname "$ZONES_BENCH_RENDERED_SCENARIO")"
rm -f -- "$ZONES_BENCH_WITHDRAWAL_BLOCKS_REPORT" "$ZONES_BENCH_WITHDRAWAL_BLOCKS_SUMMARY"
secret_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/zones-neobank-auth.XXXXXX")"
chmod 700 "$secret_dir"
export ZONES_BENCH_ZONE_AUTH_MAP="$secret_dir/zone-auth.json"
auth_pid=""
cleanup() {
    local status=$?
    [[ -z "$auth_pid" ]] || { kill -TERM "$auth_pid" 2>/dev/null || true; wait "$auth_pid" 2>/dev/null || true; }
    rm -f -- "$secret_dir/mnemonic" "$secret_dir/zone-auth.json" "$secret_dir/auth-token-map.log"
    rmdir "$secret_dir" 2>/dev/null || true
    unset ZONES_BENCH_MNEMONIC
    exit "$status"
}
trap cleanup EXIT INT TERM

run_setup_scenario() {
    local stage="$1" scenario="$2" count="$3" report="$4" context="${5:-}"
    local concurrency="$ZONES_BENCH_MAX_CONCURRENT"
    if (( 10#$concurrency > 10#$count )); then concurrency="$count"; fi

    echo "neobank stage=start run_id=$ZONES_BENCH_RUN_ID preset=$ZONES_BENCH_NEOBANK_PRESET stage=$stage${context:+ $context}"
    "$txgen_bin" scenario run \
        --scenario "$scenario" --count "$count" --starts-per-second 0 \
        --max-in-flight "$concurrency" --max-rpc-in-flight "$concurrency" \
        --failure-policy fail-fast --step-timeout "$ZONES_BENCH_STEP_TIMEOUT" \
        --seed "$ZONES_BENCH_SEED" --report "$report"
    assert_scenario_report "$report" "$count" "$stage"
    echo "neobank stage=end run_id=$ZONES_BENCH_RUN_ID preset=$ZONES_BENCH_NEOBANK_PRESET stage=$stage${context:+ $context}"
}

# Zone transaction-pool admission requires every sender to hold a nonzero
# balance of an enabled token. Run one exact encrypted onramp per benchmark
# account before the untimed outbox approvals. This setup scenario uses account
# leases, so count=accounts touches every account exactly once while respecting
# the configured max-in-flight cap.
seed_zone_admission_balances() {
    local seed_report="$ZONES_BENCH_OUTPUT/admission-seed-report.json"
    local total=$((10#$ZONES_BENCH_ACCOUNTS))

    run_setup_scenario \
        zone_admission_seed "$admission_seed_scenario" "$total" "$seed_report"
    echo "Zone admission seed verified: $total/$total exact encrypted deposits processed"
}

stage_start render_scenario
"$txgen_bin" scenario render \
    --scenario "$scenario_path" \
    --output "$ZONES_BENCH_RENDERED_SCENARIO"
[[ -s "$ZONES_BENCH_RENDERED_SCENARIO" ]] ||
    die "txgen did not render the composed private-flow scenario"
stage_end render_scenario

# The bootstrap scenario approves the control account before depositing the
# Zone fee token to the sequencer.
run_setup_scenario \
    bootstrap "$bootstrap_scenario" 1 \
    "$ZONES_BENCH_OUTPUT/bootstrap-report.json"

run_setup_scenario \
    portal_approval "$portal_approval_scenario" "$ZONES_BENCH_ACCOUNTS" \
    "$ZONES_BENCH_OUTPUT/portal-approval-report.json" "approval_round=portal"

# Deposit-only never submits a user transaction to the Zone. Every other preset
# seeds the enabled-token balance required by current Zone txpool admission.
if [[ "$ZONES_BENCH_NEOBANK_PRESET" != "encrypted-deposit" ]]; then
    seed_zone_admission_balances
fi

# The auth map is intentionally mode 0600 and is never copied to benchmark artifacts.
stage_start auth_token_map
"$txgen_bin" auth-token-map --spec "$neobank_specs/zone-flow.yml" --pool users \
    --zone-id "$ZONES_BENCH_EXPECTED_ZONE_ID" \
    --chain-id "$ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID" \
    --ttl-secs "$ZONES_BENCH_AUTH_TTL_SECS" \
    --refresh-before-secs "$ZONES_BENCH_AUTH_REFRESH_SECS" \
    --watch --output "$secret_dir/zone-auth.json" >"$secret_dir/auth-token-map.log" 2>&1 &
auth_pid=$!
for _ in $(seq 1 60); do [[ -f "$secret_dir/zone-auth.json" ]] && break; sleep 1; done
[[ -f "$secret_dir/zone-auth.json" ]] || die "timed out creating private Zone auth map"
jq -e 'to_entries | all(.[]; (.key | type) == "string" and (.value | type) == "string")' \
    "$secret_dir/zone-auth.json" >/dev/null ||
    die "private Zone auth map is malformed"
jq -S 'keys | sort' "$secret_dir/zone-auth.json" >"$ZONES_BENCH_OUTPUT/accounts.json"
jq -e --argjson expected "$ZONES_BENCH_ACCOUNTS" '
    length == $expected and
    all(.[]; type == "string") and
    (unique | length) == $expected
' "$ZONES_BENCH_OUTPUT/accounts.json" >/dev/null ||
    die "auth map did not derive the expected unique benchmark account pool"
stage_end auth_token_map

# Approvals are untimed. Deposit-only submits no user Zone transaction.
# The fixed scenario approves both enabled assets for each leased account.
if [[ "$ZONES_BENCH_NEOBANK_PRESET" != "encrypted-deposit" ]]; then
    run_setup_scenario \
        zone_approvals "$zone_approval_scenario" "$ZONES_BENCH_ACCOUNTS" \
        "$ZONES_BENCH_OUTPUT/zone-approvals-report.json" \
        "approval_round=base_and_earn"
fi

if [[ "$ZONES_BENCH_NEOBANK_PRESET" == "rewards-redemption" ]]; then
    initial_share_supply="$(read_l1_uint "$ZONES_BENCH_EARN_VAULT" 'totalEarnShares()(uint256)')"
    initial_earn_supply="$(read_l1_uint "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)')"
    [[ "$initial_share_supply" == 0 && "$initial_earn_supply" == 0 ]] ||
        die "reward position setup requires zero initial EarnToken supply"

    stage_start rewards_position_setup
    position_concurrency="$ZONES_BENCH_MAX_CONCURRENT"
    if (( 10#$position_concurrency > 10#$ZONES_BENCH_ACCOUNTS )); then
        position_concurrency="$ZONES_BENCH_ACCOUNTS"
    fi
    "$txgen_bin" scenario run \
        --scenario "$neobank_specs/rewards-position-scenario.yml" \
        --count "$ZONES_BENCH_ACCOUNTS" \
        --max-in-flight "$position_concurrency" --max-rpc-in-flight "$position_concurrency" \
        --failure-policy fail-fast --step-timeout "$ZONES_BENCH_STEP_TIMEOUT" \
        --seed "$ZONES_BENCH_SEED" \
        --report "$ZONES_BENCH_OUTPUT/rewards-position-report.json"
    assert_scenario_report \
        "$ZONES_BENCH_OUTPUT/rewards-position-report.json" \
        "$ZONES_BENCH_ACCOUNTS" "reward position setup"
    stage_end rewards_position_setup

    stage_start rewards_position_check
    positioned_share_supply="$(read_l1_uint "$ZONES_BENCH_EARN_VAULT" 'totalEarnShares()(uint256)')"
    positioned_earn_supply="$(read_l1_uint "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)')"
    [[ "$positioned_share_supply" == "$reward_total_position" ]] ||
        die "reward position share supply $positioned_share_supply does not equal $reward_total_position"
    [[ "$positioned_earn_supply" == "$reward_total_position" ]] ||
        die "reward position EarnToken supply $positioned_earn_supply does not equal $reward_total_position"
    verify_reward_zone_balances seeded "$reward_total_position" "$reward_position_per_account"
    reward_quote_before="$(read_l1_uint \
        "$ZONES_BENCH_EARN_VAULT" 'previewRedeem(uint256)(uint256)' \
        "$ZONES_BENCH_WITHDRAWAL_AMOUNT")"
    stage_end rewards_position_check

    stage_start rewards_funding
    "$txgen_bin" scenario run \
        --scenario "$neobank_specs/rewards-funding-scenario.yml" \
        --count 1 --max-in-flight 1 --max-rpc-in-flight 2 \
        --failure-policy fail-fast --step-timeout 2m \
        --seed "$ZONES_BENCH_SEED" \
        --report "$ZONES_BENCH_OUTPUT/rewards-funding-report.json"
    assert_scenario_report \
        "$ZONES_BENCH_OUTPUT/rewards-funding-report.json" 1 "reward funding setup"
    stage_end rewards_funding

    stage_start rewards_funding_check
    funded_share_supply="$(read_l1_uint "$ZONES_BENCH_EARN_VAULT" 'totalEarnShares()(uint256)')"
    funded_earn_supply="$(read_l1_uint "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)')"
    reward_quote_after="$(read_l1_uint \
        "$ZONES_BENCH_EARN_VAULT" 'previewRedeem(uint256)(uint256)' \
        "$ZONES_BENCH_WITHDRAWAL_AMOUNT")"
    [[ "$funded_share_supply" == "$reward_total_position" ]] ||
        die "reward funding changed EarnVault share supply"
    [[ "$funded_earn_supply" == "$reward_total_position" ]] ||
        die "reward funding changed EarnToken total supply"
    python3 - "$reward_quote_before" "$reward_quote_after" <<'PY'
import sys

before, after = map(int, sys.argv[1:])
if after <= before:
    raise SystemExit(f"reward funding did not increase redemption value: {before} -> {after}")
print(f"reward redemption quote increased: {before} -> {after}")
PY
    stage_end rewards_funding_check
fi

if [[ "$ZONES_BENCH_NEOBANK_PRESET" == "private-withdrawal" ||
      "$ZONES_BENCH_NEOBANK_PRESET" == "swapped-redemption" ]]; then
    initial_share_supply="$(read_l1_uint "$ZONES_BENCH_EARN_VAULT" 'totalEarnShares()(uint256)')"
    initial_earn_supply="$(read_l1_uint "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)')"
    [[ "$initial_share_supply" == 0 && "$initial_earn_supply" == 0 ]] ||
        die "$ZONES_BENCH_NEOBANK_PRESET position setup requires zero initial EarnToken supply"

    stage_start redemption_position_setup
    position_concurrency="$ZONES_BENCH_MAX_CONCURRENT"
    if (( 10#$position_concurrency > 10#$ZONES_BENCH_ACCOUNTS )); then
        position_concurrency="$ZONES_BENCH_ACCOUNTS"
    fi
    "$txgen_bin" scenario run \
        --scenario "$neobank_specs/swapped-redemption-position-scenario.yml" \
        --count "$ZONES_BENCH_ACCOUNTS" \
        --max-in-flight "$position_concurrency" --max-rpc-in-flight "$position_concurrency" \
        --failure-policy fail-fast --step-timeout "$ZONES_BENCH_STEP_TIMEOUT" \
        --seed "$ZONES_BENCH_SEED" \
        --report "$ZONES_BENCH_OUTPUT/swapped-redemption-position-report.json"
    assert_scenario_report \
        "$ZONES_BENCH_OUTPUT/swapped-redemption-position-report.json" \
        "$ZONES_BENCH_ACCOUNTS" "$ZONES_BENCH_NEOBANK_PRESET position setup"
    stage_end redemption_position_setup

    stage_start redemption_position_check
    positioned_share_supply="$(read_l1_uint "$ZONES_BENCH_EARN_VAULT" 'totalEarnShares()(uint256)')"
    positioned_earn_supply="$(read_l1_uint "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)')"
    [[ "$positioned_share_supply" == "$swapped_redemption_total_position" ]] ||
        die "$ZONES_BENCH_NEOBANK_PRESET share supply $positioned_share_supply does not equal $swapped_redemption_total_position"
    [[ "$positioned_earn_supply" == "$swapped_redemption_total_position" ]] ||
        die "$ZONES_BENCH_NEOBANK_PRESET EarnToken supply $positioned_earn_supply does not equal $swapped_redemption_total_position"
    verify_reward_zone_balances \
        seeded "$swapped_redemption_total_position" \
        "$swapped_redemption_position_per_account"
    stage_end redemption_position_check
fi

if [[ "$ZONES_BENCH_NEOBANK_PRESET" == "slippage-bounce" ]]; then
    stage_start slippage_precondition
    bounce_earn_supply="$(cast call "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)' \
        --rpc-url "$L1_RPC_URL" | awk '{print $1}')"
    bounce_vault_balance="$(cast call "$ZONES_BENCH_VAULT" 'balanceOf(address)(uint256)' \
        "$ZONES_BENCH_ENGINE" --rpc-url "$L1_RPC_URL" | awk '{print $1}')"
    [[ "$bounce_earn_supply" =~ ^[0-9]+$ && "$bounce_vault_balance" =~ ^[0-9]+$ ]] ||
        die "could not read slippage-bounce L1 preconditions"
    stage_end slippage_precondition
fi

measured_token_balance_before=""
measured_token_balance_holder=""
case "$ZONES_BENCH_NEOBANK_PRESET" in
    encrypted-deposit)
        measured_token_balance_holder="$L1_PORTAL_ADDRESS"
        measured_token_balance_before="$(read_l1_uint \
            "$ZONES_BENCH_DLUSD" 'balanceOf(address)(uint256)' \
            "$measured_token_balance_holder")"
        ;;
esac

l1_measurement_anchor_block="$(cast block-number --rpc-url "$l1_measurement_rpc")"
[[ "$l1_measurement_anchor_block" =~ ^[0-9]+$ ]] ||
    die "could not capture measured L1 start block"
l1_measurement_start_block=$((10#$l1_measurement_anchor_block + 1))
echo "neobank measured L1 start block: $l1_measurement_start_block"
stage_start private_flow
scenario_report_args=()
build_scenario_report_args scenario_report_args "$ZONES_BENCH_REPORT"
"$txgen_bin" scenario run --scenario "$scenario_path" --count "$ZONES_BENCH_COUNT" \
    --starts-per-second "$ZONES_BENCH_TPS" --max-in-flight "$ZONES_BENCH_MAX_CONCURRENT" --max-rpc-in-flight "$ZONES_BENCH_MAX_CONCURRENT" \
    --failure-policy fail-fast --step-timeout "$ZONES_BENCH_STEP_TIMEOUT" --seed "$ZONES_BENCH_SEED" \
    --sample-instances "$sample_instances" "${scenario_report_args[@]}"
l1_measurement_end_block="$(cast block-number --rpc-url "$l1_measurement_rpc")"
[[ "$l1_measurement_end_block" =~ ^[0-9]+$ ]] ||
    die "could not capture measured L1 end block"
echo "neobank measured L1 end block: $l1_measurement_end_block"
stage_end private_flow

if [[ "$ZONES_BENCH_NEOBANK_PRESET" == "slippage-bounce" ]]; then
    stage_start slippage_postcondition
    final_earn_supply="$(cast call "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)' \
        --rpc-url "$L1_RPC_URL" | awk '{print $1}')"
    final_vault_balance="$(cast call "$ZONES_BENCH_VAULT" 'balanceOf(address)(uint256)' \
        "$ZONES_BENCH_ENGINE" --rpc-url "$L1_RPC_URL" | awk '{print $1}')"
    [[ "$final_earn_supply" == "$bounce_earn_supply" ]] ||
        die "slippage bounce changed EarnToken total supply"
    [[ "$final_vault_balance" == "$bounce_vault_balance" ]] ||
        die "slippage bounce changed the engine vault balance"
    stage_end slippage_postcondition
fi

if [[ "$ZONES_BENCH_NEOBANK_PRESET" == "rewards-redemption" ]]; then
    stage_start rewards_postcondition
    final_share_supply="$(read_l1_uint "$ZONES_BENCH_EARN_VAULT" 'totalEarnShares()(uint256)')"
    final_earn_supply="$(read_l1_uint "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)')"
    [[ "$final_share_supply" == "$reward_expected_remaining" ]] ||
        die "terminal reward share supply $final_share_supply does not equal $reward_expected_remaining"
    [[ "$final_earn_supply" == "$reward_expected_remaining" ]] ||
        die "terminal reward EarnToken supply $final_earn_supply does not equal $reward_expected_remaining"
    verify_reward_zone_balances \
        remaining "$reward_expected_remaining" "$ZONES_BENCH_WITHDRAWAL_AMOUNT"
    stage_end rewards_postcondition
fi

if [[ "$ZONES_BENCH_NEOBANK_PRESET" == "private-withdrawal" ||
      "$ZONES_BENCH_NEOBANK_PRESET" == "swapped-redemption" ]]; then
    stage_start redemption_postcondition
    final_share_supply="$(read_l1_uint "$ZONES_BENCH_EARN_VAULT" 'totalEarnShares()(uint256)')"
    final_earn_supply="$(read_l1_uint "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)')"
    [[ "$final_share_supply" == "$swapped_redemption_expected_remaining" ]] ||
        die "terminal $ZONES_BENCH_NEOBANK_PRESET share supply $final_share_supply does not equal $swapped_redemption_expected_remaining"
    [[ "$final_earn_supply" == "$swapped_redemption_expected_remaining" ]] ||
        die "terminal $ZONES_BENCH_NEOBANK_PRESET EarnToken supply $final_earn_supply does not equal $swapped_redemption_expected_remaining"
    verify_reward_zone_balances \
        distributed_remaining "$swapped_redemption_expected_remaining" \
        "$ZONES_BENCH_WITHDRAWAL_AMOUNT" \
        "$swapped_redemption_position_per_account"
    stage_end redemption_postcondition
fi

assert_scenario_report "$ZONES_BENCH_REPORT" "$ZONES_BENCH_COUNT" "private flow"

if (( withdrawals_per_journey > 0 )); then
    expected_withdrawals=$((10#$ZONES_BENCH_COUNT * withdrawals_per_journey))
    expected_earn_deposits=$((10#$ZONES_BENCH_COUNT * earn_deposits_per_journey))
    expected_earn_redeems=$((10#$ZONES_BENCH_COUNT * earn_redeems_per_journey))
    expected_offramps=$((10#$ZONES_BENCH_COUNT * offramps_per_journey))
    expected_earn_deposit_callback_successes=$((10#$ZONES_BENCH_COUNT * earn_deposit_callback_successes_per_journey))
    expected_earn_redeem_callback_successes=$((10#$ZONES_BENCH_COUNT * earn_redeem_callback_successes_per_journey))
    stage_start withdrawal_capacity
    python3 "$bench_dir/collect-withdrawal-blocks.py" \
        --rpc-url "$l1_measurement_rpc" \
        --portal "$L1_PORTAL_ADDRESS" \
        --portal-abi "$bench_dir/neobank/abis/neobank-zone-portal.json" \
        --from-block "$l1_measurement_start_block" \
        --to-block "$l1_measurement_end_block" \
        --expected-withdrawals "$expected_withdrawals" \
        --earn-router "$ZONES_BENCH_EARN_ROUTER" \
        --bridge "$ZONES_BENCH_BRIDGE_WALLET" \
        --stable-token "$ZONES_BENCH_DLUSD" \
        --stable-token "$ZONES_BENCH_PATHUSD" \
        --earn-share-token "$ZONES_BENCH_EARN_TOKEN" \
        --expected-earn-deposits "$expected_earn_deposits" \
        --expected-earn-redeems "$expected_earn_redeems" \
        --expected-offramps "$expected_offramps" \
        --expected-earn-deposit-callback-successes "$expected_earn_deposit_callback_successes" \
        --expected-earn-redeem-callback-successes "$expected_earn_redeem_callback_successes" \
        --output "$ZONES_BENCH_WITHDRAWAL_BLOCKS_REPORT" \
        --markdown-output "$ZONES_BENCH_WITHDRAWAL_BLOCKS_SUMMARY"
    stage_end withdrawal_capacity
fi

if [[ -n "$measured_token_balance_before" ]]; then
    measured_token_balance_after="$(read_l1_uint \
        "$ZONES_BENCH_DLUSD" 'balanceOf(address)(uint256)' \
        "$measured_token_balance_holder")"
    case "$ZONES_BENCH_NEOBANK_PRESET" in
        encrypted-deposit) measured_balance_amount="$ZONES_BENCH_DEPOSIT_AMOUNT" ;;
        *) die "unexpected measured balance preset" ;;
    esac
    python3 - \
        "$measured_token_balance_before" "$measured_token_balance_after" \
        "$ZONES_BENCH_COUNT" "$measured_balance_amount" \
        "$ZONES_BENCH_NEOBANK_PRESET" <<'PY'
import sys

before, after, count, amount = map(int, sys.argv[1:5])
preset = sys.argv[5]
expected = count * amount
observed = after - before
if observed != expected:
    raise SystemExit(
        f"{preset} terminal L1 token delta {observed} does not equal {expected}"
    )
print(f"{preset} terminal L1 token delta verified: {observed}")
PY
fi
