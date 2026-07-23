#!/usr/bin/env bash

# Run the complete private-Zone journey. Provisioning, fixture deployment, approvals,
# and private-RPC authentication are deliberately outside the scenario measurement.
set -Eeuo pipefail

bench_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scenario-reporting.sh
source "$bench_dir/scenario-reporting.sh"

die() { echo "error: $*" >&2; exit 1; }
need() { [[ -n "${!1:-}" ]] || die "$1 must be set"; }
uint() { [[ "${!1:-}" =~ ^[0-9]+$ ]] || die "$1 must be an unsigned integer"; }

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

for name in L1_RPC_URL ZONE_RPC_URL ZONE_PRIVATE_RPC_URL \
    L1_PORTAL_ADDRESS ZONES_BENCH_TOKEN ZONES_BENCH_DLUSD ZONES_BENCH_PATHUSD ZONES_BENCH_EARN_TOKEN \
    ZONES_BENCH_GATEWAY ZONES_BENCH_BRIDGE_WALLET ZONES_BENCH_VAULT ZONES_BENCH_ENGINE \
    ZONES_BENCH_VAULT_ADAPTER ZONES_BENCH_REWARDS ZONES_BENCH_SEED
do need "$name"; done

ZONES_BENCH_CONTROL_ACCOUNT_INDEX="${ZONES_BENCH_CONTROL_ACCOUNT_INDEX:-0}"
ZONES_BENCH_ACCOUNT_START="${ZONES_BENCH_ACCOUNT_START:-16}"
ZONES_BENCH_ACCOUNTS="${ZONES_BENCH_ACCOUNTS:-100}"
ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX="${ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX:-4}"
ZONES_BENCH_COUNT="${ZONES_BENCH_COUNT:-1000}"
ZONES_BENCH_TPS="${ZONES_BENCH_TPS:-20}"
ZONES_BENCH_MAX_CONCURRENT="${ZONES_BENCH_MAX_CONCURRENT:-100}"
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
ZONES_BENCH_AUTH_TTL_SECS="${ZONES_BENCH_AUTH_TTL_SECS:-600}"
ZONES_BENCH_AUTH_REFRESH_SECS="${ZONES_BENCH_AUTH_REFRESH_SECS:-60}"
ZONES_BENCH_STEP_TIMEOUT="${ZONES_BENCH_STEP_TIMEOUT:-10m}"
ZONES_BENCH_RUN_ID="${ZONES_BENCH_RUN_ID:-local}"
ZONES_BENCH_NEOBANK_PRESET="${ZONES_BENCH_NEOBANK_PRESET:-full-journey}"
case "$ZONES_BENCH_NEOBANK_PRESET" in
    direct-lifecycle)
        scenario_file=direct-lifecycle-scenario.yml
        base_token_label=pathusd
        expected_base_token="$ZONES_BENCH_PATHUSD"
        leases_per_journey=1
        ;;
    third-party-recipient)
        scenario_file=third-party-recipient-scenario.yml
        base_token_label=pathusd
        expected_base_token="$ZONES_BENCH_PATHUSD"
        leases_per_journey=2
        ;;
    full-journey)
        scenario_file=private-flow-scenario.yml
        base_token_label=dlusd
        expected_base_token="$ZONES_BENCH_DLUSD"
        leases_per_journey=1
        ;;
    rewards-redemption)
        scenario_file=rewards-redemption-scenario.yml
        base_token_label=pathusd
        expected_base_token="$ZONES_BENCH_PATHUSD"
        leases_per_journey=1
        ;;
    slippage-bounce)
        scenario_file=slippage-bounce-scenario.yml
        base_token_label=dlusd
        expected_base_token="$ZONES_BENCH_DLUSD"
        leases_per_journey=1
        ;;
    swapped-lifecycle)
        scenario_file=swapped-lifecycle-scenario.yml
        base_token_label=dlusd
        expected_base_token="$ZONES_BENCH_DLUSD"
        leases_per_journey=1
        ;;
    *) die "unsupported neobank preset: $ZONES_BENCH_NEOBANK_PRESET" ;;
esac
[[ "${ZONES_BENCH_TOKEN,,}" == "${expected_base_token,,}" ]] ||
    die "ZONES_BENCH_TOKEN must match the $base_token_label token for $ZONES_BENCH_NEOBANK_PRESET"
for name in ZONES_BENCH_CONTROL_ACCOUNT_INDEX ZONES_BENCH_ACCOUNT_START ZONES_BENCH_ACCOUNTS ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX ZONES_BENCH_COUNT ZONES_BENCH_TPS \
    ZONES_BENCH_MAX_CONCURRENT ZONES_BENCH_DEPOSIT_AMOUNT ZONES_BENCH_ACTIVITY_AMOUNT \
    ZONES_BENCH_WITHDRAWAL_AMOUNT ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT \
    ZONES_BENCH_CALLBACK_GAS_LIMIT ZONES_BENCH_SEED
do uint "$name"; done
(( 10#$ZONES_BENCH_ACCOUNTS > 0 && 10#$ZONES_BENCH_COUNT > 0 )) || die "accounts and count must be positive"
[[ "$ZONES_BENCH_CONTROL_ACCOUNT_INDEX" == 0 ]] ||
    die "this topology fixes the neobank control account at mnemonic index 0"
[[ "$ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX" == 4 ]] ||
    die "this topology fixes the approval sponsor at mnemonic index 4"
required_accounts=$((10#$ZONES_BENCH_MAX_CONCURRENT * leases_per_journey))
(( required_accounts <= 10#$ZONES_BENCH_ACCOUNTS )) ||
    die "$ZONES_BENCH_NEOBANK_PRESET requires at least $required_accounts accounts for max-concurrent=$ZONES_BENCH_MAX_CONCURRENT"
account_journeys=$((10#$ZONES_BENCH_COUNT * leases_per_journey))
journeys_per_account=$(((account_journeys + 10#$ZONES_BENCH_ACCOUNTS - 1) / 10#$ZONES_BENCH_ACCOUNTS))

# Reward sizing uses arbitrary-precision arithmetic so configured uint128
# amounts cannot silently wrap in the shell.
reward_onramp_per_account="$ZONES_BENCH_DEPOSIT_AMOUNT"
reward_position_per_account="$ZONES_BENCH_WITHDRAWAL_AMOUNT"
reward_total_position=0
reward_fund_amount=1
reward_first_redeem_amount=1
reward_second_redeem_amount=1
reward_expected_remaining=0
reward_fund_gas_limit=5000000
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
    python3 - \
        "$ZONE_PRIVATE_RPC_URL" "$ZONES_BENCH_ZONE_AUTH_MAP" \
        "$ZONES_BENCH_OUTPUT/preflight.json" "$ZONES_BENCH_EARN_TOKEN" \
        "$ZONES_BENCH_ACCOUNTS" "$mode" "$expected_total" "$expected_unit" <<'PY'
import json
import sys
import urllib.error
import urllib.request

rpc_url, auth_path, preflight_path, token, expected_accounts, mode, expected_total, expected_unit = sys.argv[1:]
expected_accounts = int(expected_accounts)
expected_total = int(expected_total)
expected_unit = int(expected_unit)
with open(auth_path, encoding="utf-8") as handle:
    auth = json.load(handle)
with open(preflight_path, encoding="utf-8") as handle:
    accounts = json.load(handle)["accounts"]
if len(accounts) != expected_accounts:
    raise SystemExit(
        f"preflight contains {len(accounts)} accounts, expected {expected_accounts}"
    )

balances = []
for request_id, account in enumerate(accounts, 1):
    address = account["address"]
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
bench_bin="${TXGEN_BENCH_BIN:-bench}"
for command in "$txgen_bin" "$bench_bin" awk cast grep jq python3 sed; do command -v "$command" >/dev/null || die "missing $command"; done
if [[ -n "${ZONES_XTASK_BIN:-}" ]]; then preflight=("$ZONES_XTASK_BIN" benchmark-preflight); else preflight=(cargo run --profile release -p tempo-xtask -- benchmark-preflight); fi

mkdir -p "$ZONES_BENCH_OUTPUT" "$(dirname "$ZONES_BENCH_RENDERED_SCENARIO")"
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

preflight_phase() {
    local phase="$1" fixture="$2"
    local -a command=("${preflight[@]}" --l1-rpc-url "$L1_RPC_URL" --zone-rpc-url "$ZONE_RPC_URL" \
        --token "$ZONES_BENCH_TOKEN" --account-start "$ZONES_BENCH_ACCOUNT_START" \
        --accounts "$ZONES_BENCH_ACCOUNTS" --deposit-amount "$ZONES_BENCH_DEPOSIT_AMOUNT" \
        --activity-amount "$ZONES_BENCH_ACTIVITY_AMOUNT" --withdrawal-amount "$ZONES_BENCH_WITHDRAWAL_AMOUNT" \
        --bootstrap-deposit-amount "$ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT" --transactions-per-account "$journeys_per_account" \
        --sponsored-approval-rounds 2 \
        --check-phase "$phase" --output "$ZONES_BENCH_OUTPUT")
    [[ -z "$fixture" ]] || command+=(--fixture-state "$fixture")
    "${command[@]}"
}

# Send one expiring-nonce approval per account. Each round is generated only
# after bootstrap and immediately before it is sent, so slow provisioning cannot
# consume the validity window. txgen gives setup steps a shared inclusion key,
# which is right for dependent deployment setup but serializes unrelated account
# approvals; drop only that synthetic barrier.
send_zone_approval_round() {
    local token_label="$1" token="$2"
    local spec="$ZONES_BENCH_OUTPUT/zone-approval-${token_label}.yml"
    local raw="$ZONES_BENCH_OUTPUT/zone-approval-${token_label}.serial.ndjson"
    local stream="$ZONES_BENCH_OUTPUT/zone-approval-${token_label}.ndjson"
    local actual

    {
        printf 'chain_id: %s\n\n' "$zone_chain_id"
        printf 'gas:\n  max_fee_per_gas: %s\n  max_priority_fee_per_gas: %s\n\n' "$zone_fee" "$zone_fee"
        printf 'accounts:\n  users:\n    mnemonic: "${ZONES_BENCH_MNEMONIC}"\n    range: [%s, %s]\n  sponsor:\n    mnemonic: "${ZONES_BENCH_MNEMONIC}"\n    index: %s\n\n' "$ZONES_BENCH_ACCOUNT_START" "$account_end" "$ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX"
        printf 'artifacts:\n  TIP20: txgen/abis/tip20.json\n\nsetup:\n  steps:\n'
        for ((index = 0; index < 10#$ZONES_BENCH_ACCOUNTS; index++)); do
        printf '    - id: approve-%s-%s\n      tx:\n        type: tempo\n        from: { pool: users, select: { index: %s } }\n        sponsor: { pool: sponsor, select: { index: 0 } }\n        expiring_nonce: true\n        valid_for_secs: 25\n        gas_limit: 500000\n        fee_token: "%s"\n        call:\n          to: "%s"\n          abi: TIP20\n          function: "approve(address,uint256)"\n          args: ["0x1c00000000000000000000000000000000000002", "115792089237316195423570985008687907853269984665640564039457584007913129639935"]\n' "$token_label" "$index" "$index" "$ZONES_BENCH_TOKEN" "$token"
        done
        # The txgen generator requires a positive mix even with --count 0.
        printf '\ntemplates:\n  approval_probe:\n    type: tempo\n    from: { pool: users, select: { index: 0 } }\n    sponsor: { pool: sponsor, select: { index: 0 } }\n    expiring_nonce: true\n    valid_for_secs: 25\n    gas_limit: 500000\n    fee_token: "%s"\n    call:\n      to: "%s"\n      abi: TIP20\n      function: "approve(address,uint256)"\n      args: ["0x1c00000000000000000000000000000000000002", "0"]\nmix:\n  - template: approval_probe\n    weight: 1\n' "$ZONES_BENCH_TOKEN" "$token"
    } >"$spec"

    echo "neobank stage=start run_id=$ZONES_BENCH_RUN_ID stage=zone_approval approval_round=$token_label"
    echo "Zone $token_label approval setup: generating $ZONES_BENCH_ACCOUNTS expiring-nonce transactions"
    "$txgen_bin" generate --spec "$spec" --count 0 --seed "$ZONES_BENCH_SEED" --output "$raw"
    actual="$(jq -s -r 'length' "$raw")"
    [[ "$actual" == "$ZONES_BENCH_ACCOUNTS" ]] ||
        die "Zone $token_label approval setup rendered $actual transactions; expected $ZONES_BENCH_ACCOUNTS"
    jq -e -s --argjson expected "$ZONES_BENCH_ACCOUNTS" '
        length == $expected and
        all(.[]; .phase == "setup" and (.submission_keys | length) == 1 and (.inclusion_keys | length) == 1) and
        ([.[].submission_keys[]] | unique | length) == $expected and
        ([.[].inclusion_keys[]] | unique | length) == 1
    ' "$raw" >/dev/null || die "Zone $token_label approval setup has invalid scheduling keys"
    jq -c '.inclusion_keys = []' "$raw" >"$stream"
    rm -f -- "$raw"

    echo "Zone $token_label approval setup: sending up to $ZONES_BENCH_MAX_CONCURRENT concurrently"
    "$bench_bin" send --input "$stream" --rpc-url "$ZONE_PRIVATE_RPC_URL" --query-rpc-url "$ZONE_RPC_URL" \
        --sender-header-name X-Authorization-Token --sender-header-map "$secret_dir/zone-auth.json" \
        --tps 0 --max-concurrent "$ZONES_BENCH_MAX_CONCURRENT" --retries 0 --drain-timeout 0 --report console
    echo "neobank stage=end run_id=$ZONES_BENCH_RUN_ID stage=zone_approval approval_round=$token_label"
}

# The bootstrap gives the sequencer the preset's Zone fee token for sponsored,
# untimed Zone approvals.
stage_start bootstrap
preflight_phase bootstrap empty
"$txgen_bin" scenario run --scenario "$ZONES_BENCH_OUTPUT/bootstrap-scenario.yml" --count 1 \
    --max-in-flight 1 --max-rpc-in-flight 4 --failure-policy fail-fast --seed "$ZONES_BENCH_SEED" \
    --report "$ZONES_BENCH_OUTPUT/bootstrap-report.json"
stage_end bootstrap
# Refresh preflight after bootstrap so the rendered report reflects its funded
# sponsor state. Setup approvals themselves are deliberately non-expiring.
stage_start post_bootstrap_preflight
preflight_phase bootstrap ""
stage_end post_bootstrap_preflight

# The generic preflight renders one portal approval per user. It is outside timing.
stage_start portal_approval
"$txgen_bin" generate --spec "$ZONES_BENCH_OUTPUT/deposit.yml" --count 0 --seed "$ZONES_BENCH_SEED" \
    --output "$ZONES_BENCH_OUTPUT/portal-approvals.ndjson"
"$bench_bin" send --input "$ZONES_BENCH_OUTPUT/portal-approvals.ndjson" --rpc-url "$L1_RPC_URL" \
    --query-rpc-url "$L1_RPC_URL" --tps 0 --max-concurrent "$ZONES_BENCH_MAX_CONCURRENT" --retries 0 --drain-timeout 0 --report console
stage_end portal_approval

stage_start render_scenario
cp -R contrib/bench/neobank "$ZONES_BENCH_OUTPUT/neobank"
mkdir -p "$ZONES_BENCH_OUTPUT/txgen"
cp -R contrib/bench/txgen/abis "$ZONES_BENCH_OUTPUT/txgen/abis"
# Preflight already renders its portal artifacts into this directory. Copy the
# fixture artifacts into that existing directory rather than nesting them at
# abis/abis/, which would leave ZoneGateway unresolved by the scenario loader.
cp contrib/bench/neobank/abis/*.json "$ZONES_BENCH_OUTPUT/abis/"
zone_id="$(jq -er '.zoneId' "$ZONES_BENCH_OUTPUT/preflight.json")"
l1_chain_id="$(cast chain-id --rpc-url "$L1_RPC_URL")"
zone_chain_id="$(cast chain-id --rpc-url "$ZONE_RPC_URL")"
l1_fee="$(cast gas-price --rpc-url "$L1_RPC_URL")"
zone_fee="$(cast gas-price --rpc-url "$ZONE_RPC_URL")"
account_end=$((10#$ZONES_BENCH_ACCOUNT_START + 10#$ZONES_BENCH_ACCOUNTS))
control_account_end=$((10#$ZONES_BENCH_CONTROL_ACCOUNT_INDEX + 1))
sequencer_account_end=$((10#$ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX + 1))
render_sources=(l1-onramp.yml zone-flow.yml scenario-fragments.yml "$scenario_file")
if [[ "$ZONES_BENCH_NEOBANK_PRESET" == "rewards-redemption" ]]; then
    render_sources+=(rewards-position-scenario.yml rewards-funding-scenario.yml)
fi
rendered_documents=()
for source in "${render_sources[@]}"; do
    destination="$source"
    [[ "$source" != "$scenario_file" ]] || destination=private-flow-scenario.yml
    rendered_documents+=("$ZONES_BENCH_OUTPUT/$destination")
    sed \
        -e "s|__L1_CHAIN_ID__|$l1_chain_id|g" -e "s|__ZONE_CHAIN_ID__|$zone_chain_id|g" \
        -e "s|__ZONE_ID__|$zone_id|g" -e "s|__ACCOUNT_START__|$ZONES_BENCH_ACCOUNT_START|g" -e "s|__ACCOUNT_END__|$account_end|g" \
        -e "s|__CONTROL_ACCOUNT_INDEX__|$ZONES_BENCH_CONTROL_ACCOUNT_INDEX|g" -e "s|__CONTROL_ACCOUNT_END__|$control_account_end|g" \
        -e "s|__SEQUENCER_ACCOUNT_INDEX__|$ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX|g" -e "s|__SEQUENCER_ACCOUNT_END__|$sequencer_account_end|g" \
        -e "s|__L1_MAX_FEE_PER_GAS__|$l1_fee|g" -e "s|__L1_MAX_PRIORITY_FEE_PER_GAS__|$l1_fee|g" \
        -e "s|__ZONE_MAX_FEE_PER_GAS__|$zone_fee|g" -e "s|__ZONE_MAX_PRIORITY_FEE_PER_GAS__|$zone_fee|g" \
        -e "s|__PORTAL__|$L1_PORTAL_ADDRESS|g" -e "s|__INBOX__|0x1c00000000000000000000000000000000000001|g" -e "s|__OUTBOX__|0x1c00000000000000000000000000000000000002|g" \
        -e "s|__ZONE_TOKEN__|$ZONES_BENCH_TOKEN|g" \
        -e "s|__DLUSD__|$ZONES_BENCH_DLUSD|g" -e "s|__PATHUSD__|$ZONES_BENCH_PATHUSD|g" -e "s|__EARN_TOKEN__|$ZONES_BENCH_EARN_TOKEN|g" \
        -e "s|__GATEWAY__|$ZONES_BENCH_GATEWAY|g" -e "s|__BRIDGE_WALLET__|$ZONES_BENCH_BRIDGE_WALLET|g" -e "s|__REWARDS__|$ZONES_BENCH_REWARDS|g" \
        -e "s|__ONRAMP_AMOUNT__|$ZONES_BENCH_DEPOSIT_AMOUNT|g" -e "s|__PRIVATE_TRANSFER_AMOUNT__|$ZONES_BENCH_ACTIVITY_AMOUNT|g" \
        -e "s|__EARN_DEPOSIT_AMOUNT__|$ZONES_BENCH_WITHDRAWAL_AMOUNT|g" -e "s|__EARN_REDEEM_AMOUNT__|$ZONES_BENCH_WITHDRAWAL_AMOUNT|g" \
        -e "s|__OFFRAMP_AMOUNT__|$ZONES_BENCH_ACTIVITY_AMOUNT|g" -e "s|__CALLBACK_GAS_LIMIT__|$ZONES_BENCH_CALLBACK_GAS_LIMIT|g" \
        -e "s|__REWARD_ONRAMP_PER_ACCOUNT__|$reward_onramp_per_account|g" -e "s|__REWARD_POSITION_PER_ACCOUNT__|$reward_position_per_account|g" \
        -e "s|__REWARD_FUND_AMOUNT__|$reward_fund_amount|g" -e "s|__REWARD_FUND_GAS_LIMIT__|$reward_fund_gas_limit|g" \
        -e "s|__REWARD_FIRST_REDEEM_AMOUNT__|$reward_first_redeem_amount|g" -e "s|__REWARD_SECOND_REDEEM_AMOUNT__|$reward_second_redeem_amount|g" \
        -e 's|__DEPOSIT_GAS_LIMIT__|2000000|g' -e 's|__ACTIVITY_GAS_LIMIT__|500000|g' -e 's|__WITHDRAWAL_TX_GAS_LIMIT__|10000000|g' \
        "$ZONES_BENCH_OUTPUT/neobank/$source" >"$ZONES_BENCH_OUTPUT/$destination"
done
if grep -En '__[A-Z0-9_]+__' "${rendered_documents[@]}"
then
    die "unresolved placeholder in rendered private-flow spec"
fi
# Run the composed source document so txgen retains fragment provenance in the
# report. Results consume the deterministic flattened copy below.
"$txgen_bin" scenario render \
    --scenario "$ZONES_BENCH_OUTPUT/private-flow-scenario.yml" \
    --output "$ZONES_BENCH_RENDERED_SCENARIO"
[[ -s "$ZONES_BENCH_RENDERED_SCENARIO" ]] ||
    die "txgen did not render the composed private-flow scenario"
if [[ "$ZONES_BENCH_NEOBANK_PRESET" == "rewards-redemption" ]]; then
    for setup in rewards-position rewards-funding; do
        "$txgen_bin" scenario render \
            --scenario "$ZONES_BENCH_OUTPUT/${setup}-scenario.yml" \
            --output "$ZONES_BENCH_OUTPUT/${setup}-scenario.rendered.yml"
        [[ -s "$ZONES_BENCH_OUTPUT/${setup}-scenario.rendered.yml" ]] ||
            die "txgen did not render the composed $setup scenario"
    done
fi
stage_end render_scenario

# The auth map is intentionally mode 0600 and is never copied to benchmark artifacts.
stage_start auth_token_map
"$txgen_bin" auth-token-map --spec "$ZONES_BENCH_OUTPUT/zone-flow.yml" --pool users --zone-id "$zone_id" \
    --chain-id "$zone_chain_id" --ttl-secs "$ZONES_BENCH_AUTH_TTL_SECS" --refresh-before-secs "$ZONES_BENCH_AUTH_REFRESH_SECS" \
    --watch --output "$secret_dir/zone-auth.json" >"$secret_dir/auth-token-map.log" 2>&1 &
auth_pid=$!
for _ in $(seq 1 60); do [[ -f "$secret_dir/zone-auth.json" ]] && break; sleep 1; done
[[ -f "$secret_dir/zone-auth.json" ]] || die "timed out creating private Zone auth map"
stage_end auth_token_map

# Approve both Zone assets before timing. EarnToken needs no user balance for approval;
# the sequencer sponsors these setup transactions from its untimed bootstrap balance.
send_zone_approval_round "$base_token_label" "$ZONES_BENCH_TOKEN"
send_zone_approval_round earn "$ZONES_BENCH_EARN_TOKEN"

if [[ "$ZONES_BENCH_NEOBANK_PRESET" == "rewards-redemption" ]]; then
    control_address="$(cast wallet address \
        --mnemonic "$ZONES_BENCH_MNEMONIC_FILE" \
        --mnemonic-index "$ZONES_BENCH_CONTROL_ACCOUNT_INDEX")"
    control_pathusd_balance="$(read_l1_uint "$ZONES_BENCH_PATHUSD" 'balanceOf(address)(uint256)' "$control_address")"
    initial_share_supply="$(read_l1_uint "$ZONES_BENCH_VAULT_ADAPTER" 'shareSupply()(uint256)')"
    initial_earn_supply="$(read_l1_uint "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)')"
    [[ "$initial_share_supply" == 0 && "$initial_earn_supply" == 0 ]] ||
        die "reward position setup requires zero initial EarnToken supply"

    python3 - \
        "$ZONES_BENCH_ACCOUNTS" "$ZONES_BENCH_COUNT" "$journeys_per_account" \
        "$reward_onramp_per_account" "$reward_position_per_account" "$reward_fund_amount" \
        "$control_pathusd_balance" "$l1_fee" "$zone_fee" "$reward_fund_gas_limit" <<'PY'
import sys

a, j, n, onramp, position, reward, control_balance, l1_fee, zone_fee, fund_gas = map(
    int, sys.argv[1:]
)
scale = 10**12
withdrawal_gas = 10_000_000
approval_gas = 2_000_000

def fee(gas_limit: int, gas_price: int) -> int:
    return (gas_limit * gas_price + scale - 1) // scale

# Each scenario engine starts its own expiring-fee uniqueness counter. Reserve
# the worst setup bump and every one of the 2*N measured redemptions at the
# measured run's worst global bump.
setup_fee = fee(withdrawal_gas, zone_fee + a)
measured_fee = fee(withdrawal_gas, zone_fee + 2 * j)
zone_required = setup_fee + 2 * n * measured_fee
zone_reserve = onramp - position
if zone_reserve < zone_required:
    raise SystemExit(
        f"per-account pathUSD fee reserve {zone_reserve} is below required cap {zone_required}"
    )

control_required = reward + fee(approval_gas, l1_fee) + fee(fund_gas, l1_fee)
if control_balance < control_required:
    raise SystemExit(
        f"control pathUSD balance {control_balance} is below reward plus fee cap {control_required}"
    )
print(
    "reward setup capacity verified: "
    f"per-account Zone reserve={zone_reserve}/{zone_required}, "
    f"control L1 balance={control_balance}/{control_required}"
)
PY

    stage_start rewards_position_setup
    position_concurrency="$ZONES_BENCH_MAX_CONCURRENT"
    if (( 10#$position_concurrency > 10#$ZONES_BENCH_ACCOUNTS )); then
        position_concurrency="$ZONES_BENCH_ACCOUNTS"
    fi
    "$txgen_bin" scenario run \
        --scenario "$ZONES_BENCH_OUTPUT/rewards-position-scenario.yml" \
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
    positioned_share_supply="$(read_l1_uint "$ZONES_BENCH_VAULT_ADAPTER" 'shareSupply()(uint256)')"
    positioned_earn_supply="$(read_l1_uint "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)')"
    [[ "$positioned_share_supply" == "$reward_total_position" ]] ||
        die "reward position share supply $positioned_share_supply does not equal $reward_total_position"
    [[ "$positioned_earn_supply" == "$reward_total_position" ]] ||
        die "reward position EarnToken supply $positioned_earn_supply does not equal $reward_total_position"
    verify_reward_zone_balances seeded "$reward_total_position" "$reward_position_per_account"
    reward_quote_before="$(read_l1_uint \
        "$ZONES_BENCH_VAULT_ADAPTER" 'previewRedeem(uint256)(uint256)' \
        "$ZONES_BENCH_WITHDRAWAL_AMOUNT")"
    stage_end rewards_position_check

    stage_start rewards_funding
    "$txgen_bin" scenario run \
        --scenario "$ZONES_BENCH_OUTPUT/rewards-funding-scenario.yml" \
        --count 1 --max-in-flight 1 --max-rpc-in-flight 2 \
        --failure-policy fail-fast --step-timeout 2m \
        --seed "$ZONES_BENCH_SEED" \
        --report "$ZONES_BENCH_OUTPUT/rewards-funding-report.json"
    assert_scenario_report \
        "$ZONES_BENCH_OUTPUT/rewards-funding-report.json" 1 "reward funding setup"
    stage_end rewards_funding

    stage_start rewards_funding_check
    funded_share_supply="$(read_l1_uint "$ZONES_BENCH_VAULT_ADAPTER" 'shareSupply()(uint256)')"
    funded_earn_supply="$(read_l1_uint "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)')"
    reward_quote_after="$(read_l1_uint \
        "$ZONES_BENCH_VAULT_ADAPTER" 'previewRedeem(uint256)(uint256)' \
        "$ZONES_BENCH_WITHDRAWAL_AMOUNT")"
    [[ "$funded_share_supply" == "$reward_total_position" ]] ||
        die "reward funding changed VaultAdapter share supply"
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

stage_start private_flow
scenario_report_args=()
build_scenario_report_args scenario_report_args "$ZONES_BENCH_REPORT"
"$txgen_bin" scenario run --scenario "$ZONES_BENCH_OUTPUT/private-flow-scenario.yml" --count "$ZONES_BENCH_COUNT" \
    --starts-per-second "$ZONES_BENCH_TPS" --max-in-flight "$ZONES_BENCH_MAX_CONCURRENT" --max-rpc-in-flight "$ZONES_BENCH_MAX_CONCURRENT" \
    --failure-policy fail-fast --step-timeout "$ZONES_BENCH_STEP_TIMEOUT" --seed "$ZONES_BENCH_SEED" "${scenario_report_args[@]}"
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
    final_share_supply="$(read_l1_uint "$ZONES_BENCH_VAULT_ADAPTER" 'shareSupply()(uint256)')"
    final_earn_supply="$(read_l1_uint "$ZONES_BENCH_EARN_TOKEN" 'totalSupply()(uint256)')"
    [[ "$final_share_supply" == "$reward_expected_remaining" ]] ||
        die "terminal reward share supply $final_share_supply does not equal $reward_expected_remaining"
    [[ "$final_earn_supply" == "$reward_expected_remaining" ]] ||
        die "terminal reward EarnToken supply $final_earn_supply does not equal $reward_expected_remaining"
    verify_reward_zone_balances \
        remaining "$reward_expected_remaining" "$ZONES_BENCH_WITHDRAWAL_AMOUNT"
    stage_end rewards_postcondition
fi

assert_scenario_report "$ZONES_BENCH_REPORT" "$ZONES_BENCH_COUNT" "private flow"
