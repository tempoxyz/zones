#!/usr/bin/env bash

# Build txgen scenario reporter arguments without placing ClickHouse credentials
# in argv. The endpoint is credential-free; authentication stays in txgen's
# CLICKHOUSE_USER and CLICKHOUSE_PASSWORD environment variables.
build_scenario_report_args() {
    local destination_name="$1"
    local json_report="$2"
    local -n destination="$destination_name"
    local name

    destination=(--report "$json_report")
    if [[ -z "${CLICKHOUSE_URL:-}" ]]; then
        if [[ "${ZONES_BENCH_REQUIRE_CLICKHOUSE:-0}" == 1 ]]; then
            echo "error: CLICKHOUSE_URL is required for this benchmark run" >&2
            return 1
        fi
        return 0
    fi

    for name in \
        CLICKHOUSE_USER CLICKHOUSE_PASSWORD \
        ZONES_BENCH_ZONES_REF ZONES_BENCH_GIT_REF ZONES_BENCH_PHASE
    do
        if [[ -z "${!name:-}" ]]; then
            echo "error: $name is required when ClickHouse reporting is enabled" >&2
            return 1
        fi
    done

    destination+=(
        --report "clickhouse:$CLICKHOUSE_URL"
        --metadata "git-sha=$ZONES_BENCH_ZONES_REF"
        --metadata "git-ref=$ZONES_BENCH_GIT_REF"
        --metadata "phase=$ZONES_BENCH_PHASE"
    )
    [[ -z "${ZONES_BENCH_NEOBANK_PRESET:-}" || "$ZONES_BENCH_NEOBANK_PRESET" == none ]] ||
        destination+=(--metadata "neobank-preset=$ZONES_BENCH_NEOBANK_PRESET")
    [[ -z "${ZONES_BENCH_RUN_ID:-}" ]] ||
        destination+=(--metadata "zones-run-id=$ZONES_BENCH_RUN_ID")
    [[ -z "${ZONES_BENCH_BLOAT_GIB:-}" ]] ||
        destination+=(--metadata "state-bloat-gib=$ZONES_BENCH_BLOAT_GIB")
    [[ -z "${ZONES_BENCH_GITHUB_REPOSITORY:-}" ]] ||
        destination+=(--metadata "github-repository=$ZONES_BENCH_GITHUB_REPOSITORY")
    [[ -z "${ZONES_BENCH_GITHUB_RUN_ID:-}" ]] ||
        destination+=(--metadata "github-run-id=$ZONES_BENCH_GITHUB_RUN_ID")
    [[ -z "${ZONES_BENCH_GITHUB_WORKFLOW:-}" ]] ||
        destination+=(--metadata "github-workflow-file=$ZONES_BENCH_GITHUB_WORKFLOW")
    [[ -z "${ZONES_BENCH_GITHUB_PR_NUMBER:-}" ]] ||
        destination+=(--metadata "github-pr-number=$ZONES_BENCH_GITHUB_PR_NUMBER")
}
