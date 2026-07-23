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
ZONES_BENCH_PHASE=roundtrip
ZONES_BENCH_NEOBANK_PRESET=none
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
[[ "$joined" == *" --metadata github-run-id=123 "* ]]
[[ "$joined" == *" --metadata github-pr-number=742 "* ]]
[[ "$joined" != *"fixture-user"* ]]
[[ "$joined" != *"fixture-password"* ]]

echo "scenario reporting helper tests passed"
