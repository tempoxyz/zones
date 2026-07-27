#!/usr/bin/env bash

set -Eeuo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=../scenario-reporting.sh
source "$root/contrib/bench/scenario-reporting.sh"

unset CLICKHOUSE_URL CLICKHOUSE_USER CLICKHOUSE_PASSWORD
unset ZONES_BENCH_REQUIRE_CLICKHOUSE
args=()
build_scenario_report_args args /tmp/report.json
[[ "${args[*]}" == "--report /tmp/report.json" ]]

ZONES_BENCH_REQUIRE_CLICKHOUSE=1
if build_scenario_report_args args /tmp/report.json 2>/dev/null; then
    echo "required ClickHouse reporting accepted missing configuration" >&2
    exit 1
fi

CLICKHOUSE_URL=https://clickhouse.invalid:8443
CLICKHOUSE_USER=fixture-user
CLICKHOUSE_PASSWORD=fixture-password
ZONES_BENCH_ZONES_REF=1111111111111111111111111111111111111111
ZONES_BENCH_GIT_REF=dan/example
ZONES_BENCH_PHASE=neobank-e2e
ZONES_BENCH_NEOBANK_PRESET=full-journey
ZONES_BENCH_SWAP_MECHANISM=simple
ZONES_BENCH_RECIPIENT_MODE=random
ZONES_BENCH_ACCOUNTS=100
ZONES_BENCH_COUNT=1000
ZONES_BENCH_TPS=1.2
ZONES_BENCH_MAX_CONCURRENT=12
ZONES_BENCH_DEPOSIT_AMOUNT=2000000
ZONES_BENCH_ACTIVITY_AMOUNT=1
ZONES_BENCH_WITHDRAWAL_AMOUNT=1000000
ZONES_BENCH_BOOTSTRAP_DEPOSIT_AMOUNT=10000000
ZONES_BENCH_SWAP_LIQUIDITY=10000000000
ZONES_BENCH_CALLBACK_GAS_LIMIT=10000000
ZONES_BENCH_L1_GAS_LIMIT=30000000
ZONES_BENCH_L1_GENERAL_GAS_LIMIT=30000000
ZONES_BENCH_WITHDRAWAL_MAX_BATCH_GAS=30000000
ZONES_BENCH_WITHDRAWAL_MAX_IN_FLIGHT_BATCHES=12
ZONES_BENCH_ZONE_BATCH_INTERVAL_BLOCKS=120
ZONES_BENCH_WITHDRAWAL_POLL_INTERVAL_SECS=5
ZONES_BENCH_STEP_TIMEOUT=10m
ZONES_BENCH_SETUP_SETTLEMENT_TIMEOUT_SECS=120
ZONES_BENCH_DRAIN_TIMEOUT=300
ZONES_BENCH_SEED=123456
ZONES_BENCH_RUN_ID=123-1
ZONES_BENCH_BLOAT_GIB=1
ZONES_BENCH_GITHUB_REPOSITORY=tempoxyz/zones
ZONES_BENCH_GITHUB_RUN_ID=123
ZONES_BENCH_GITHUB_WORKFLOW=zones-benchmark.yml
ZONES_BENCH_GITHUB_PR_NUMBER=742
build_scenario_report_args args /tmp/report.json

joined=" ${args[*]} "
[[ "$joined" == *" --report clickhouse:https://clickhouse.invalid:8443 "* ]]
[[ "$joined" == *" --metadata git-sha=$ZONES_BENCH_ZONES_REF "* ]]
[[ "$joined" == *" --metadata git-ref=dan/example "* ]]
[[ "$joined" == *" --metadata neobank-preset=full-journey "* ]]
[[ "$joined" == *" --metadata swap-mechanism=simple "* ]]
[[ "$joined" == *" --metadata recipient-mode=random "* ]]
[[ "$joined" == *" --metadata accounts=100 "* ]]
[[ "$joined" == *" --metadata count=1000 "* ]]
[[ "$joined" == *" --metadata target-rate=1.2 "* ]]
[[ "$joined" == *" --metadata max-concurrent=12 "* ]]
[[ "$joined" == *" --metadata swap-liquidity=10000000000 "* ]]
[[ "$joined" == *" --metadata callback-gas-limit=10000000 "* ]]
[[ "$joined" == *" --metadata l1-gas-limit=30000000 "* ]]
[[ "$joined" == *" --metadata l1-general-gas-limit=30000000 "* ]]
[[ "$joined" == *" --metadata withdrawal-max-batch-gas=30000000 "* ]]
[[ "$joined" == *" --metadata withdrawal-max-in-flight-batches=12 "* ]]
[[ "$joined" == *" --metadata zone-batch-interval-blocks=120 "* ]]
[[ "$joined" == *" --metadata withdrawal-poll-interval-secs=5 "* ]]
[[ "$joined" == *" --metadata step-timeout=10m "* ]]
[[ "$joined" == *" --metadata setup-settlement-timeout-secs=120 "* ]]
[[ "$joined" == *" --metadata drain-timeout-secs=300 "* ]]
[[ "$joined" == *" --metadata seed=123456 "* ]]
[[ "$joined" == *" --metadata state-bloat-gib=1 "* ]]
[[ "$joined" == *" --metadata github-run-id=123 "* ]]
[[ "$joined" == *" --metadata github-pr-number=742 "* ]]
[[ "$joined" != *"fixture-user"* ]]
[[ "$joined" != *"fixture-password"* ]]

echo "scenario reporting helper tests passed"
