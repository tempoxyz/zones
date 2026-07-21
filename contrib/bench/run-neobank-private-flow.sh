#!/usr/bin/env bash

# Run the complete private-Zone journey. Provisioning, fixture deployment, approvals,
# and private-RPC authentication are deliberately outside the scenario measurement.
set -Eeuo pipefail

die() { echo "error: $*" >&2; exit 1; }
need() { [[ -n "${!1:-}" ]] || die "$1 must be set"; }
uint() { [[ "${!1:-}" =~ ^[0-9]+$ ]] || die "$1 must be an unsigned integer"; }

for name in ZONES_BENCH_MNEMONIC L1_RPC_URL ZONE_RPC_URL ZONE_PRIVATE_RPC_URL \
    L1_PORTAL_ADDRESS ZONES_BENCH_DLUSD ZONES_BENCH_PATHUSD ZONES_BENCH_EARN_TOKEN \
    ZONES_BENCH_GATEWAY ZONES_BENCH_BRIDGE_WALLET ZONES_BENCH_SEED
do need "$name"; done

ZONES_BENCH_ACCOUNT_START="${ZONES_BENCH_ACCOUNT_START:-16}"
ZONES_BENCH_ACCOUNTS="${ZONES_BENCH_ACCOUNTS:-100}"
ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX="${ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX:-4}"
ZONES_BENCH_COUNT="${ZONES_BENCH_COUNT:-100}"
ZONES_BENCH_TPS="${ZONES_BENCH_TPS:-10}"
ZONES_BENCH_MAX_CONCURRENT="${ZONES_BENCH_MAX_CONCURRENT:-100}"
ZONES_BENCH_DEPOSIT_AMOUNT="${ZONES_BENCH_DEPOSIT_AMOUNT:-2000000}"
ZONES_BENCH_ACTIVITY_AMOUNT="${ZONES_BENCH_ACTIVITY_AMOUNT:-1}"
ZONES_BENCH_WITHDRAWAL_AMOUNT="${ZONES_BENCH_WITHDRAWAL_AMOUNT:-1000000}"
ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT="${ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT:-10000000}"
ZONES_BENCH_CALLBACK_GAS_LIMIT="${ZONES_BENCH_CALLBACK_GAS_LIMIT:-2000000}"
ZONES_BENCH_OUTPUT="${ZONES_BENCH_OUTPUT:-target/zones-benchmark/neobank-e2e}"
ZONES_BENCH_REPORT="${ZONES_BENCH_REPORT:-target/zones-benchmark/report-neobank-e2e.json}"
ZONES_BENCH_AUTH_TTL_SECS="${ZONES_BENCH_AUTH_TTL_SECS:-600}"
ZONES_BENCH_AUTH_REFRESH_SECS="${ZONES_BENCH_AUTH_REFRESH_SECS:-60}"
ZONES_BENCH_STEP_TIMEOUT="${ZONES_BENCH_STEP_TIMEOUT:-10m}"
for name in ZONES_BENCH_ACCOUNT_START ZONES_BENCH_ACCOUNTS ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX ZONES_BENCH_COUNT ZONES_BENCH_TPS \
    ZONES_BENCH_MAX_CONCURRENT ZONES_BENCH_DEPOSIT_AMOUNT ZONES_BENCH_ACTIVITY_AMOUNT \
    ZONES_BENCH_WITHDRAWAL_AMOUNT ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT \
    ZONES_BENCH_CALLBACK_GAS_LIMIT ZONES_BENCH_SEED
do uint "$name"; done
(( 10#$ZONES_BENCH_ACCOUNTS > 0 && 10#$ZONES_BENCH_COUNT > 0 )) || die "accounts and count must be positive"
(( 10#$ZONES_BENCH_MAX_CONCURRENT <= 10#$ZONES_BENCH_ACCOUNTS )) || die "max-concurrent cannot exceed accounts"

txgen_bin="${TXGEN_TEMPO_BIN:-txgen-tempo}"
bench_bin="${TXGEN_BENCH_BIN:-bench}"
for command in "$txgen_bin" "$bench_bin" cast grep jq sed; do command -v "$command" >/dev/null || die "missing $command"; done
if [[ -n "${ZONES_XTASK_BIN:-}" ]]; then preflight=("$ZONES_XTASK_BIN" benchmark-preflight); else preflight=(cargo run --profile release -p tempo-xtask -- benchmark-preflight); fi

mkdir -p "$ZONES_BENCH_OUTPUT"
secret_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/zones-neobank-auth.XXXXXX")"
chmod 700 "$secret_dir"
auth_pid=""
cleanup() {
    local status=$?
    [[ -z "$auth_pid" ]] || { kill -TERM "$auth_pid" 2>/dev/null || true; wait "$auth_pid" 2>/dev/null || true; }
    rm -f -- "$secret_dir/mnemonic" "$secret_dir/zone-auth.json"
    rmdir "$secret_dir" 2>/dev/null || true
    exit "$status"
}
trap cleanup EXIT INT TERM

preflight_phase() {
    local phase="$1" fixture="$2"
    local -a command=("${preflight[@]}" --l1-rpc-url "$L1_RPC_URL" --zone-rpc-url "$ZONE_RPC_URL" \
        --token "$ZONES_BENCH_DLUSD" --account-start "$ZONES_BENCH_ACCOUNT_START" \
        --accounts "$ZONES_BENCH_ACCOUNTS" --deposit-amount "$ZONES_BENCH_DEPOSIT_AMOUNT" \
        --activity-amount "$ZONES_BENCH_ACTIVITY_AMOUNT" --withdrawal-amount "$ZONES_BENCH_WITHDRAWAL_AMOUNT" \
        --bootstrap-deposit-amount "$ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT" --transactions-per-account 1 \
        --check-phase "$phase" --output "$ZONES_BENCH_OUTPUT")
    [[ -z "$fixture" ]] || command+=(--fixture-state "$fixture")
    "${command[@]}"
}

# The bootstrap gives the sequencer DLUSD for sponsored, untimed Zone approvals.
preflight_phase bootstrap empty
"$txgen_bin" scenario run --scenario "$ZONES_BENCH_OUTPUT/bootstrap-scenario.yml" --count 1 \
    --max-in-flight 1 --max-rpc-in-flight 4 --failure-policy fail-fast --seed "$ZONES_BENCH_SEED" \
    --report "$ZONES_BENCH_OUTPUT/bootstrap-report.json"
# Refresh preflight after bootstrap so the rendered report reflects its funded
# sponsor state. Setup approvals themselves are deliberately non-expiring.
preflight_phase bootstrap ""

# The generic preflight renders one portal approval per user. It is outside timing.
"$txgen_bin" generate --spec "$ZONES_BENCH_OUTPUT/deposit.yml" --count 0 --seed "$ZONES_BENCH_SEED" \
    --output "$ZONES_BENCH_OUTPUT/portal-approvals.ndjson"
"$bench_bin" send --input "$ZONES_BENCH_OUTPUT/portal-approvals.ndjson" --rpc-url "$L1_RPC_URL" \
    --query-rpc-url "$L1_RPC_URL" --tps 0 --max-concurrent "$ZONES_BENCH_MAX_CONCURRENT" --retries 0 --drain-timeout 0 --report console

cp -R contrib/bench/neobank "$ZONES_BENCH_OUTPUT/neobank"
mkdir -p "$ZONES_BENCH_OUTPUT/txgen"
cp -R contrib/bench/txgen/abis "$ZONES_BENCH_OUTPUT/txgen/abis"
cp -R contrib/bench/neobank/abis "$ZONES_BENCH_OUTPUT/abis"
zone_id="$(jq -er '.zoneId' "$ZONES_BENCH_OUTPUT/preflight.json")"
l1_chain_id="$(cast chain-id --rpc-url "$L1_RPC_URL")"
zone_chain_id="$(cast chain-id --rpc-url "$ZONE_RPC_URL")"
l1_fee="$(cast gas-price --rpc-url "$L1_RPC_URL")"
zone_fee="$(cast gas-price --rpc-url "$ZONE_RPC_URL")"
account_end=$((10#$ZONES_BENCH_ACCOUNT_START + 10#$ZONES_BENCH_ACCOUNTS))
sequencer_account_end=$((10#$ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX + 1))
for file in l1-onramp.yml zone-flow.yml private-flow-scenario.yml; do
    sed \
        -e "s|__L1_CHAIN_ID__|$l1_chain_id|g" -e "s|__ZONE_CHAIN_ID__|$zone_chain_id|g" \
        -e "s|__ZONE_ID__|$zone_id|g" -e "s|__ACCOUNT_START__|$ZONES_BENCH_ACCOUNT_START|g" -e "s|__ACCOUNT_END__|$account_end|g" \
        -e "s|__SEQUENCER_ACCOUNT_INDEX__|$ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX|g" -e "s|__SEQUENCER_ACCOUNT_END__|$sequencer_account_end|g" \
        -e "s|__L1_MAX_FEE_PER_GAS__|$l1_fee|g" -e "s|__L1_MAX_PRIORITY_FEE_PER_GAS__|$l1_fee|g" \
        -e "s|__ZONE_MAX_FEE_PER_GAS__|$zone_fee|g" -e "s|__ZONE_MAX_PRIORITY_FEE_PER_GAS__|$zone_fee|g" \
        -e "s|__PORTAL__|$L1_PORTAL_ADDRESS|g" -e "s|__INBOX__|0x1c00000000000000000000000000000000000001|g" -e "s|__OUTBOX__|0x1c00000000000000000000000000000000000002|g" \
        -e "s|__DLUSD__|$ZONES_BENCH_DLUSD|g" -e "s|__PATHUSD__|$ZONES_BENCH_PATHUSD|g" -e "s|__EARN_TOKEN__|$ZONES_BENCH_EARN_TOKEN|g" \
        -e "s|__GATEWAY__|$ZONES_BENCH_GATEWAY|g" -e "s|__BRIDGE_WALLET__|$ZONES_BENCH_BRIDGE_WALLET|g" \
        -e "s|__ONRAMP_AMOUNT__|$ZONES_BENCH_DEPOSIT_AMOUNT|g" -e "s|__PRIVATE_TRANSFER_AMOUNT__|$ZONES_BENCH_ACTIVITY_AMOUNT|g" \
        -e "s|__EARN_DEPOSIT_AMOUNT__|$ZONES_BENCH_WITHDRAWAL_AMOUNT|g" -e "s|__EARN_REDEEM_AMOUNT__|$ZONES_BENCH_WITHDRAWAL_AMOUNT|g" \
        -e "s|__OFFRAMP_AMOUNT__|$ZONES_BENCH_ACTIVITY_AMOUNT|g" -e "s|__CALLBACK_GAS_LIMIT__|$ZONES_BENCH_CALLBACK_GAS_LIMIT|g" \
        -e 's|__DEPOSIT_GAS_LIMIT__|2000000|g' -e 's|__ACTIVITY_GAS_LIMIT__|500000|g' -e 's|__WITHDRAWAL_TX_GAS_LIMIT__|10000000|g' \
        "$ZONES_BENCH_OUTPUT/neobank/$file" >"$ZONES_BENCH_OUTPUT/$file"
done
if grep -En '__[A-Z0-9_]+__' \
    "$ZONES_BENCH_OUTPUT/l1-onramp.yml" \
    "$ZONES_BENCH_OUTPUT/zone-flow.yml" \
    "$ZONES_BENCH_OUTPUT/private-flow-scenario.yml"
then
    die "unresolved placeholder in rendered private-flow spec"
fi

# The auth map is intentionally mode 0600 and is never copied to benchmark artifacts.
"$txgen_bin" auth-token-map --spec "$ZONES_BENCH_OUTPUT/zone-flow.yml" --pool users --zone-id "$zone_id" \
    --chain-id "$zone_chain_id" --ttl-secs "$ZONES_BENCH_AUTH_TTL_SECS" --refresh-before-secs "$ZONES_BENCH_AUTH_REFRESH_SECS" \
    --watch --output "$secret_dir/zone-auth.json" >"$ZONES_BENCH_OUTPUT/auth-token-map.log" 2>&1 &
auth_pid=$!
for _ in $(seq 1 60); do [[ -f "$secret_dir/zone-auth.json" ]] && break; sleep 1; done
[[ -f "$secret_dir/zone-auth.json" ]] || die "timed out creating private Zone auth map"
export ZONES_BENCH_ZONE_AUTH_MAP="$secret_dir/zone-auth.json"

# Approve both Zone assets before timing. EarnToken needs no user balance for approval;
# the sequencer sponsors these setup transactions from its untimed bootstrap balance.
zone_approval_spec="$ZONES_BENCH_OUTPUT/zone-approvals.yml"
{
    printf 'chain_id: %s\n\n' "$zone_chain_id"
    printf 'gas:\n  max_fee_per_gas: %s\n  max_priority_fee_per_gas: %s\n\n' "$zone_fee" "$zone_fee"
    printf 'accounts:\n  users:\n    mnemonic: "${ZONES_BENCH_MNEMONIC}"\n    range: [%s, %s]\n  sponsor:\n    mnemonic: "${ZONES_BENCH_MNEMONIC}"\n    index: 4\n\n' "$ZONES_BENCH_ACCOUNT_START" "$account_end"
    printf 'artifacts:\n  TIP20: %s/txgen/abis/tip20.json\n\nsetup:\n  steps:\n' "$ZONES_BENCH_OUTPUT"
    for ((index = 0; index < 10#$ZONES_BENCH_ACCOUNTS; index++)); do
        for token in "$ZONES_BENCH_DLUSD" "$ZONES_BENCH_EARN_TOKEN"; do
            printf '    - id: approve-%s-%s\n      tx:\n        type: tempo\n        from: { pool: users, select: { index: %s } }\n        sponsor: { pool: sponsor, select: { index: 0 } }\n        gas_limit: 500000\n        fee_token: "%s"\n        call:\n          to: "%s"\n          abi: TIP20\n          function: "approve(address,uint256)"\n          args: ["0x1c00000000000000000000000000000000000002", "115792089237316195423570985008687907853269984665640564039457584007913129639935"]\n' "$index" "${token: -1}" "$index" "$ZONES_BENCH_DLUSD" "$token"
        done
    done
    printf '\ntemplates: {}\nmix: []\n'
} >"$zone_approval_spec"
"$txgen_bin" generate --spec "$zone_approval_spec" --count 0 --seed "$ZONES_BENCH_SEED" --output "$ZONES_BENCH_OUTPUT/zone-approvals.ndjson"
"$bench_bin" send --input "$ZONES_BENCH_OUTPUT/zone-approvals.ndjson" --rpc-url "$ZONE_PRIVATE_RPC_URL" --query-rpc-url "$ZONE_RPC_URL" \
    --sender-header-name X-Authorization-Token --sender-header-map "$secret_dir/zone-auth.json" \
    --tps 0 --max-concurrent "$ZONES_BENCH_MAX_CONCURRENT" --retries 0 --drain-timeout 0 --report console

"$txgen_bin" scenario run --scenario "$ZONES_BENCH_OUTPUT/private-flow-scenario.yml" --count "$ZONES_BENCH_COUNT" \
    --starts-per-second "$ZONES_BENCH_TPS" --max-in-flight "$ZONES_BENCH_MAX_CONCURRENT" --max-rpc-in-flight "$ZONES_BENCH_MAX_CONCURRENT" \
    --failure-policy continue --step-timeout "$ZONES_BENCH_STEP_TIMEOUT" --seed "$ZONES_BENCH_SEED" --report "$ZONES_BENCH_REPORT"
