#!/usr/bin/env bash
# Run one isolated leg after the controller restores both virgin L1 snapshots.
set -Eeuo pipefail
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
trap '"$script_dir/provision-topology.sh" cleanup' EXIT
"$script_dir/provision-topology.sh" up
set -a
# shellcheck source=/dev/null
source "$ZONES_BENCH_ENV_FILE"
set +a
"$script_dir/run-neobank-private-flow.sh"
# Validate the execution range with the matching prover utilities binary.
# shellcheck source=/dev/null
source "$ZONES_BENCH_SPF_RANGE"
private_key="$(cast wallet private-key --mnemonic "$ZONES_BENCH_MNEMONIC_FILE" --mnemonic-index 4)"
PRIVATE_KEY="$private_key" "$SPF_BIN" generate-input \
    --tempo-rpc-url "$ZONES_BENCH_L1_QUERY_RPC_URL" \
    --chain "$ZONES_BENCH_ZONE_GENESIS" \
    --zone-private-rpc-url "$ZONE_PRIVATE_RPC_URL" \
    --zone-unrestricted-rpc-url "$ZONE_RPC_URL" \
    --from-block "$ZONES_BENCH_SPF_FROM_BLOCK" \
    --to-block "$ZONES_BENCH_SPF_TO_BLOCK" \
    --output "$ZONES_BENCH_SPF_OUTPUT"
unset private_key
test -s "$ZONES_BENCH_SPF_OUTPUT"
