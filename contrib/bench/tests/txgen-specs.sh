#!/usr/bin/env bash

set -Eeuo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
txgen_bin="${TXGEN_TEMPO_BIN:-txgen-tempo}"

command -v "$txgen_bin" >/dev/null || {
    echo "txgen-tempo binary not found: $txgen_bin" >&2
    exit 1
}

if [[ -z "${ZONES_BENCH_MNEMONIC:-}" ]]; then
    mnemonic_file="${ZONES_BENCH_MNEMONIC_FILE:-}"
    [[ -n "$mnemonic_file" && -f "$mnemonic_file" ]] || {
        echo "ZONES_BENCH_MNEMONIC or ZONES_BENCH_MNEMONIC_FILE must be set" >&2
        exit 1
    }
    export ZONES_BENCH_MNEMONIC
    ZONES_BENCH_MNEMONIC="$(tr -d '\r\n' <"$mnemonic_file")"
fi

[[ -n "$ZONES_BENCH_MNEMONIC" ]] || {
    echo "benchmark mnemonic must not be empty" >&2
    exit 1
}
if [[ "$ZONES_BENCH_MNEMONIC" == \
    "test test test test test test test test test test test junk" ]]; then
    echo "refusing the public test mnemonic" >&2
    exit 1
fi

workdir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/zones-txgen-specs.XXXXXX")"
cleanup() {
    rm -rf -- "$workdir"
    unset ZONES_BENCH_MNEMONIC
}
trap cleanup EXIT

export L1_RPC_URL=http://127.0.0.1:18545
export ZONES_BENCH_L1_QUERY_RPC_URL=http://127.0.0.1:28545
export L1_WS_RPC_URL=ws://127.0.0.1:18546
export ZONE_PRIVATE_RPC_URL=http://127.0.0.1:18547
export ZONE_RPC_URL=http://127.0.0.1:28547
export ZONE_WS_RPC_URL=ws://127.0.0.1:18548
export ZONES_BENCH_ZONE_AUTH_MAP="$workdir/zone-auth.json"
auth_map_marker=fixture-auth-token-must-not-be-rendered
printf '{"0x1111111111111111111111111111111111111111":"%s"}\n' \
    "$auth_map_marker" >"$ZONES_BENCH_ZONE_AUTH_MAP"
chmod 600 "$ZONES_BENCH_ZONE_AUTH_MAP"

export ZONES_BENCH_EXPECTED_L1_CHAIN_ID=1337
export ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID=421700001
export ZONES_BENCH_EXPECTED_ZONE_ID=1
export ZONES_BENCH_ACCOUNT_START=16
export ZONES_BENCH_ACCOUNT_END=18
export ZONES_BENCH_RECIPIENT_ACCOUNT_START=1000000
export ZONES_BENCH_RECIPIENT_ACCOUNT_END=1000002
export ZONES_BENCH_CONTROL_ACCOUNT_INDEX=0
export ZONES_BENCH_CONTROL_ACCOUNT_END=1
export ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX=4
export ZONES_BENCH_SEQUENCER_ACCOUNT_END=5
export ZONES_BENCH_SEQUENCER_ADDRESS=0x3000000000000000000000000000000000000006

export ZONES_BENCH_L1_MAX_FEE_PER_GAS=10000000000
export ZONES_BENCH_L1_MAX_PRIORITY_FEE_PER_GAS=0
export ZONES_BENCH_ZONE_MAX_FEE_PER_GAS=10000000000
export ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS=0
export ZONES_BENCH_APPROVAL_GAS_LIMIT=500000
export ZONES_BENCH_DEPOSIT_GAS_LIMIT=2000000
export ZONES_BENCH_ACTIVITY_GAS_LIMIT=500000
export ZONES_BENCH_WITHDRAWAL_TX_GAS_LIMIT=10000000
export ZONES_BENCH_CALLBACK_GAS_LIMIT=10000000

export L1_PORTAL_ADDRESS=0x3000000000000000000000000000000000000001
export ZONES_BENCH_INBOX=0x1c00000000000000000000000000000000000001
export ZONES_BENCH_OUTBOX=0x1c00000000000000000000000000000000000002
export ZONES_BENCH_TOKEN=0x20c0000000000000000000000000000000000001
export ZONES_BENCH_DLUSD="$ZONES_BENCH_TOKEN"
export ZONES_BENCH_PATHUSD=0x20c0000000000000000000000000000000000002
export ZONES_BENCH_EARN_TOKEN=0x20c0000000000000000000000000000000000003
export ZONES_BENCH_EARN_ROUTER=0x3000000000000000000000000000000000000002
export ZONES_BENCH_EARN_VAULT=0x3000000000000000000000000000000000000003
export ZONES_BENCH_BRIDGE_WALLET=0x3000000000000000000000000000000000000004
export ZONES_BENCH_EARN_CONTRIBUTION_CONTROLLER=0x3000000000000000000000000000000000000005

export ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT=10000000
export ZONES_BENCH_DEPOSIT_AMOUNT=2000000
export ZONES_BENCH_ACTIVITY_AMOUNT=1
export ZONES_BENCH_WITHDRAWAL_AMOUNT=1000000
export ZONES_BENCH_ADMISSION_SEED_AMOUNT=12000000
export ZONES_BENCH_REWARD_ONRAMP_PER_ACCOUNT=2000
export ZONES_BENCH_REWARD_POSITION_PER_ACCOUNT=1000
export ZONES_BENCH_REWARD_FUND_AMOUNT=10000
export ZONES_BENCH_REWARD_MAX_EARN_SHARE_SUPPLY=100000
export ZONES_BENCH_REWARD_FIRST_REDEEM_AMOUNT=40
export ZONES_BENCH_REWARD_SECOND_REDEEM_AMOUNT=60
export ZONES_BENCH_SWAPPED_REDEMPTION_ONRAMP_PER_ACCOUNT=3000
export ZONES_BENCH_SWAPPED_REDEMPTION_POSITION_PER_ACCOUNT=2000
export ZONES_BENCH_PRIVATE_TRANSFER_RECIPIENT='{ var: recipient.address }'
export ZONES_BENCH_RECIPIENT_GENERATOR='{ pool: { pool: users, select: random } }'
export ZONES_BENCH_RECIPIENT_POOL=users
export ZONES_BENCH_RECIPIENT_SELECT=random

scenarios=(
    "$root"/contrib/bench/txgen/*-scenario.yml
    "$root"/contrib/bench/neobank/*-scenario.yml
)

validated=0
for scenario in "${scenarios[@]}"; do
    suite="$(basename -- "$(dirname -- "$scenario")")"
    output="$workdir/$suite-$(basename -- "$scenario")"
    "$txgen_bin" scenario validate --scenario "$scenario"
    "$txgen_bin" scenario render --scenario "$scenario" --output "$output"
    [[ -s "$output" ]] || {
        echo "scenario render was empty: $scenario" >&2
        exit 1
    }
    if grep -Fq -- "$ZONES_BENCH_MNEMONIC" "$output"; then
        echo "scenario render exposed the benchmark mnemonic: $scenario" >&2
        exit 1
    fi
    if grep -Fq -- "$auth_map_marker" "$output"; then
        echo "scenario render exposed Zone authorization data: $scenario" >&2
        exit 1
    fi
    validated=$((validated + 1))
done

approval_scenario="$root/contrib/bench/neobank/zone-outbox-approvals-scenario.yml"
approval_workload="$root/contrib/bench/neobank/zone-flow.yml"
approval_render="$workdir/neobank-zone-outbox-approvals-scenario.yml"

grep -Eq '^[[:space:]]+execution: dag$' "$approval_render" || {
    echo "Zone approval scenario must use DAG execution" >&2
    exit 1
}
[[ "$(grep -Ec '^[[:space:]]+- id: approve_(base|earn)_token$' \
    "$approval_scenario")" -eq 2 ]] || {
    echo "Zone approval scenario must contain both approval steps" >&2
    exit 1
}
if grep -Eq '^[[:space:]]+depends_on:' "$approval_scenario"; then
    echo "Zone approval steps must be independent" >&2
    exit 1
fi
[[ "$(grep -Fc 'from: { var: account.ref }' "$approval_scenario")" -eq 2 ]] || {
    echo "Zone approval steps must use the same leased account" >&2
    exit 1
}
for template in approve_base_token approve_earn_token; do
    grep -Fq "template: $template" "$approval_scenario" || {
        echo "Zone approval scenario does not submit $template" >&2
        exit 1
    }
done

assert_expiring_approval_template() {
    local template="$1"
    local block="$workdir/$template.yml"
    awk -v template="$template" '
        $0 == "  " template ":" {
            found = 1
            capture = 1
            next
        }
        capture && /^  [[:alnum:]_-]+:$/ {
            exit
        }
        capture {
            print
        }
        END {
            if (!found) {
                exit 2
            }
        }
    ' "$approval_workload" >"$block"
    grep -Fq 'from: { var: account.ref }' "$block"
    grep -Eq '^[[:space:]]+expiring_nonce: true$' "$block"
}

assert_expiring_approval_template approve_base_token
assert_expiring_approval_template approve_earn_token
[[ "$ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS" == 0 ]]
grep -Fq \
    'max_priority_fee_per_gas: ${ZONES_BENCH_ZONE_MAX_PRIORITY_FEE_PER_GAS}' \
    "$approval_workload" || {
    echo "Zone approval transactions must use zero priority fee" >&2
    exit 1
}

approval_stream="$workdir/neobank-zone-approvals.ndjson"
"$txgen_bin" generate \
    --spec "$approval_workload" \
    --count 2 \
    --seed 42 \
    --output "$approval_stream"
jq -e -s '
    length == 2 and
    ([.[].sender] | unique | length) == 1 and
    all(.[];
        .phase == "workload" and
        (.submission_keys | length) == 1 and
        (.raw | type) == "string" and
        (.raw | startswith("0x"))
    ) and
    ([.[].submission_keys[0]] | unique | length) == 2 and
    ([.[].raw] | unique | length) == 2
' "$approval_stream" >/dev/null || {
    echo "Zone approval pair did not produce distinct same-account expiring transactions" >&2
    exit 1
}

generated=0
for recipient_mode in existing random; do
    case "$recipient_mode" in
        existing)
            export ZONES_BENCH_RECIPIENT_GENERATOR='{ pool: { pool: users, select: random } }'
            export ZONES_BENCH_RECIPIENT_POOL=users
            export ZONES_BENCH_RECIPIENT_SELECT=random
            ;;
        random)
            export ZONES_BENCH_RECIPIENT_GENERATOR=random
            export ZONES_BENCH_RECIPIENT_POOL=recipients
            export ZONES_BENCH_RECIPIENT_SELECT=lease
            ;;
    esac

    for spec_name in deposit zone-activity withdrawal; do
        spec="$root/contrib/bench/txgen/$spec_name.yml"
        output="$workdir/$recipient_mode-$spec_name.ndjson"
        "$txgen_bin" generate --spec "$spec" --count 1 --output "$output"
        [[ -s "$output" ]] || {
            echo "transaction generation was empty: $recipient_mode $spec" >&2
            exit 1
        }
        generated=$((generated + 1))
    done
done

echo "validated and rendered $validated txgen scenarios"
echo "generated $generated representative transaction streams"
