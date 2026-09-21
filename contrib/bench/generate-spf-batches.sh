#!/usr/bin/env bash
# Validate every submitted batch overlapping the measured Zone block range.
set -euo pipefail

die() { echo "SPF batches: $*" >&2; exit 1; }

from=${1:?first measured block required}
to=${2:?last measured block required}
[[ "$from" =~ ^[1-9][0-9]*$ && "$to" =~ ^[1-9][0-9]*$ ]] || die "invalid measured range"
(( from <= to )) || die "reversed measured range"
wait_secs=${ZONES_BENCH_SPF_WAIT_TIMEOUT_SECS:-120}
[[ "$wait_secs" =~ ^[0-9]+$ ]] || die "invalid submission wait timeout"
output=${ZONES_BENCH_SPF_OUTPUT:?}
# An old success marker must never mask a failed profiled command.
[[ ! -e "$output" ]] || die "output already exists: $output"
mkdir -p "$(dirname "$output")"
batch_dir=$(mktemp -d "${output%.json}-batches.XXXXXX")
index="$batch_dir/index.jsonl"
touch "$index"
cursor=$from
batch_count=0
user_transactions=0

while (( cursor <= to )); do
    witness="$batch_dir/block-$cursor.json"
    report="$batch_dir/block-$cursor.txt"
    log="$batch_dir/block-$cursor.log"
    deadline=$((SECONDS + wait_secs))
    while true; do
        if "$SPF_BIN" generate-input \
            --tempo-rpc-url "$ZONES_BENCH_L1_QUERY_RPC_URL" \
            --chain "$ZONES_BENCH_ZONE_GENESIS" \
            --zone-rpc-url "$ZONE_RPC_URL" \
            --block "$cursor" --output "$witness" > "$report" 2> "$log"
        then
            break
        fi
        # Only pending submission is retryable; replay/validation errors fail immediately.
        if ! grep -Fq "batch containing Zone block $cursor has not been submitted yet" "$log" ||
           (( SECONDS >= deadline )); then
            cat "$log" >&2
            die "generation failed for batch containing block $cursor"
        fi
        sleep 1
    done
    cat "$log" >&2
    cat "$report"
    # Validate coverage from the successfully validated witness, not human-readable logs.
    bounds=$(jq -er '
        .zoneBlocks | select(type == "array" and length > 0) |
        select(all(.[]; (.number | type == "number" and . == floor) and
                       (.transactions | type == "array"))) |
        select([.[].number] == [range(.[0].number; .[-1].number + 1)]) |
        [.[0].number, .[-1].number] | @tsv
    ' "$witness") || die "invalid witness coverage for block $cursor"
    read -r batch_from batch_to <<< "$bounds"
    (( batch_from <= cursor && cursor <= batch_to )) || die "batch does not contain block $cursor"
    if (( batch_count > 0 && batch_from != cursor )); then
        die "overlapping or discontinuous batches at block $cursor"
    fi
    measured_transactions=$(jq --argjson from "$from" --argjson to "$to" '
        [.zoneBlocks[] | select(.number >= $from and .number <= $to) |
         .transactions | length] | add // 0
    ' "$witness")
    user_transactions=$((user_transactions + measured_transactions))
    jq -n --arg witness "$witness" --argjson from "$batch_from" --argjson to "$batch_to" \
        --argjson count "$measured_transactions" \
        '{fromBlock: $from, toBlock: $to, witness: $witness, measuredUserTransactions: $count}' >> "$index"
    batch_count=$((batch_count + 1))
    cursor=$((batch_to + 1))
done

if [[ "${ZONES_BENCH_NEOBANK_PRESET:-}" != encrypted-deposit ]] && (( user_transactions == 0 )); then
    die "measured range contains no Zone user transactions"
fi
# Publish only after every overlapping batch has passed native SPF validation.
jq -s --argjson from "$from" --argjson to "$to" --argjson count "$user_transactions" \
    '{fromBlock: $from, toBlock: $to, userTransactions: $count, batches: .}' \
    "$index" > "$batch_dir/manifest.json"
mv "$batch_dir/manifest.json" "$output"
echo "Validated $batch_count submitted batches covering measured Zone blocks $from..=$to ($user_transactions user transactions)"
