#!/usr/bin/env bash
set -euo pipefail
script="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/generate-spf-batches.sh"
test_dir=$(mktemp -d)
trap 'rm -rf -- "$test_dir"' EXIT

# Stub the CLI, not batch selection: exercise the actual helper and JSON handling.
mock_spf() {
    [[ "$1" == generate-input ]] || return 2
    shift
    local block output start end
    while (( $# )); do
        case "$1" in
            --block) block=$2 ;;
            --output) output=$2 ;;
            --tempo-rpc-url|--chain|--zone-rpc-url) ;;
            *) echo "unexpected CLI argument $1" >&2; return 2 ;;
        esac
        shift 2
    done
    printf '%s\n' "$block" >> "$TEST_CASE_DIR/calls"
    if [[ "$TEST_MODE" == pending || ( "$TEST_MODE" == retry && ! -e "$TEST_CASE_DIR/retried" ) ]]; then
        touch "$TEST_CASE_DIR/retried"
        echo "batch containing Zone block $block has not been submitted yet (last submitted block: 0)" >&2
        return 1
    fi
    if [[ "$TEST_MODE" == failure && "$block" == 3 ]]; then
        echo 'generated witness failed SPF validation: invalid batch shape' >&2
        return 1
    fi
    [[ "$TEST_MODE" != missing ]] || return 0
    case "$block" in
        2) start=1; end=2 ;;
        3) start=3; end=4 ;;
        5) start=5; end=7 ;;
        *) return 3 ;;
    esac
    [[ "$TEST_MODE" != gap ]] || start=$((block + 1))
    if [[ "$TEST_MODE" == overlap && "$block" == 3 ]]; then start=2; fi
    jq -n --argjson start "$start" --argjson end "$end" --arg mode "$TEST_MODE" '
        {zoneBlocks: [range($start; $end + 1) |
          {number: ., transactions: (if $mode == "empty" then [] else ["0x00"] end)}]}
    ' > "$output"
    echo 'Generated and validated SPF input'
}
export -f mock_spf
export SPF_BIN=mock_spf ZONES_BENCH_L1_QUERY_RPC_URL=http://l1
export ZONES_BENCH_ZONE_GENESIS=genesis.json ZONE_RPC_URL=http://zone
export ZONES_BENCH_NEOBANK_PRESET=full-journey

for mode in success retry pending failure missing gap overlap empty; do
    export TEST_MODE=$mode TEST_CASE_DIR="$test_dir/$mode"
    mkdir -p "$TEST_CASE_DIR"
    export ZONES_BENCH_SPF_OUTPUT="$TEST_CASE_DIR/input.json"
    export ZONES_BENCH_SPF_WAIT_TIMEOUT_SECS=0
    [[ "$mode" != retry ]] || export ZONES_BENCH_SPF_WAIT_TIMEOUT_SECS=5
    if bash "$script" 2 6 > "$TEST_CASE_DIR/stdout" 2> "$TEST_CASE_DIR/stderr"; then
        [[ "$mode" == success || "$mode" == retry ]] || { echo "unexpected success: $mode"; exit 1; }
        jq -e '.fromBlock == 2 and .toBlock == 6 and .userTransactions == 5 and
               ([.batches[].fromBlock] == [1,3,5]) and ([.batches[].toBlock] == [2,4,7])' \
            "$ZONES_BENCH_SPF_OUTPUT" >/dev/null
    else
        [[ "$mode" != success && "$mode" != retry ]] || { cat "$TEST_CASE_DIR/stderr"; exit 1; }
        [[ ! -e "$ZONES_BENCH_SPF_OUTPUT" ]]
    fi
    echo "PASS: $mode"
done

export TEST_MODE=empty ZONES_BENCH_NEOBANK_PRESET=encrypted-deposit
export TEST_CASE_DIR="$test_dir/deposit" ZONES_BENCH_SPF_OUTPUT="$test_dir/deposit/input.json"
mkdir -p "$TEST_CASE_DIR"
bash "$script" 2 6 >/dev/null
jq -e '.userTransactions == 0 and (.batches | length) == 3' "$ZONES_BENCH_SPF_OUTPUT" >/dev/null
echo 'PASS: encrypted deposit permits zero user transactions'
if bash "$script" 2 6 >/dev/null 2>&1; then echo 'accepted stale output'; exit 1; fi
echo 'PASS: stale output rejected'

export ZONES_BENCH_SPF_OUTPUT="$test_dir/invalid.json"
for args in '0 6' '6 2' 'bad 6'; do
    # Intentional splitting of literal test arguments.
    if bash "$script" $args >/dev/null 2>&1; then echo "accepted range: $args"; exit 1; fi
done
echo 'PASS: invalid ranges rejected'
