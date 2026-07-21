#!/usr/bin/env bash

set -Eeuo pipefail

readonly TEST_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../l1-snapshot.sh
source "$TEST_SCRIPT_DIR/../l1-snapshot.sh"

for command in jq sha256sum; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "error: required command not found: $command" >&2
        exit 1
    }
done

TEST_TMP="$(mktemp -d "${TMPDIR:-/tmp}/zones-l1-snapshot-test.XXXXXX")"
cleanup() {
    [[ -n "${TEST_TMP:-}" && "$TEST_TMP" == "${TMPDIR:-/tmp}/zones-l1-snapshot-test."* ]] \
        && rm -rf -- "$TEST_TMP"
}
trap cleanup EXIT

tests_run=0

pass() {
    tests_run=$((tests_run + 1))
    echo "ok $tests_run - $1"
}

fail() {
    echo "not ok $((tests_run + 1)) - $1" >&2
    if [[ -s "$TEST_TMP/last-command.log" ]]; then
        sed 's/^/  /' "$TEST_TMP/last-command.log" >&2
    fi
    exit 1
}

expect_success() {
    local label="$1"
    shift
    : >"$TEST_TMP/last-command.log"
    if ("$@") >"$TEST_TMP/last-command.log" 2>&1; then
        pass "$label"
    else
        fail "$label"
    fi
}

expect_failure() {
    local label="$1"
    shift
    : >"$TEST_TMP/last-command.log"
    if ("$@") >"$TEST_TMP/last-command.log" 2>&1; then
        fail "$label"
    else
        pass "$label"
    fi
}

write_manifest() {
    local path="$1"
    local role="$2"
    local generation="$3"
    local patched_sha raw_sha allocation_sha
    patched_sha="$(l1_snapshot_sha256 "$L1_SNAPSHOT_PATCHED_GENESIS")"
    raw_sha="$(l1_snapshot_sha256 "$L1_SNAPSHOT_RAW_GENESIS")"
    allocation_sha="$(l1_snapshot_allocation_hash)"

    jq -nS \
        --argjson schema "$L1_SNAPSHOT_SCHEMA" \
        --arg role "$role" \
        --arg cacheKey "$L1_SNAPSHOT_CACHE_KEY" \
        --arg generationId "$generation" \
        --arg patchedGenesisSha256 "$patched_sha" \
        --arg rawGenesisSha256 "$raw_sha" \
        --arg factoryGenesisAllocationSha256 "$allocation_sha" \
        --argjson config "$L1_SNAPSHOT_EXPECTED_CONFIG" '
        {
          schema: $schema,
          role: $role,
          cacheKey: $cacheKey,
          generationId: $generationId,
          patchedGenesisSha256: $patchedGenesisSha256,
          rawGenesisSha256: $rawGenesisSha256,
          factoryGenesisAllocationSha256: $factoryGenesisAllocationSha256,
          config: $config
        }
    ' >"$path"
}

setup_valid_pair() {
    local fixture="$TEST_TMP/fixture"
    rm -rf -- "$fixture"

    L1_SNAPSHOT_STATE_A_ROOT="$fixture/a/zones-l1-fixture"
    L1_SNAPSHOT_STATE_B_ROOT="$fixture/b/zones-l1-fixture"
    L1_SNAPSHOT_LOCALNET_DIR="$L1_SNAPSHOT_STATE_A_ROOT/localnet"
    L1_SNAPSHOT_RAW_GENESIS="$L1_SNAPSHOT_LOCALNET_DIR/genesis.json"
    L1_SNAPSHOT_PATCHED_GENESIS="$L1_SNAPSHOT_STATE_A_ROOT/tempo-genesis.json"
    L1_SNAPSHOT_A_DB="$L1_SNAPSHOT_STATE_A_ROOT/l1-a"
    L1_SNAPSHOT_B_DB="$L1_SNAPSHOT_STATE_B_ROOT/l1-b"
    L1_SNAPSHOT_A_MANIFEST="$L1_SNAPSHOT_STATE_A_ROOT/manifest.json"
    L1_SNAPSHOT_B_MANIFEST="$L1_SNAPSHOT_STATE_B_ROOT/manifest.json"

    local validator_a="$L1_SNAPSHOT_LOCALNET_DIR/127.0.0.2:8000"
    local validator_b="$L1_SNAPSHOT_LOCALNET_DIR/127.0.0.3:8100"
    mkdir -p \
        "$validator_a" "$validator_b" \
        "$L1_SNAPSHOT_A_DB/db" "$L1_SNAPSHOT_A_DB/static_files" \
        "$L1_SNAPSHOT_B_DB/db" "$L1_SNAPSHOT_B_DB/static_files"
    touch \
        "$validator_a/signing.key" "$validator_a/signing.share" \
        "$validator_a/enode.key" "$validator_a/enode.identity" \
        "$validator_b/signing.key" "$validator_b/signing.share" \
        "$validator_b/enode.key" "$validator_b/enode.identity"

    jq -n '{config: {chainId: 1337}, alloc: {}}' >"$L1_SNAPSHOT_RAW_GENESIS"
    jq -n \
        --arg factory "$L1_SNAPSHOT_ZONE_FACTORY" \
        --arg verifier "$L1_SNAPSHOT_VERIFIER" \
        --arg messenger "$L1_SNAPSHOT_MESSENGER" \
        --arg history "$L1_SNAPSHOT_HISTORY" '
        {
          config: {chainId: 1337, generalGasLimit: 30000000},
          alloc: {
            ($factory): {code: "0x6000"},
            ($verifier): {code: "0x6001"},
            ($messenger): {code: "0x6002"},
            ($history): {code: "0x6003"}
          }
        }
    ' >"$L1_SNAPSHOT_PATCHED_GENESIS"

    L1_SNAPSHOT_EXPECTED_CONFIG="$(jq -cnS '
        {
          schema: 1,
          tempo: {revision: "fixture"},
          l1: {chainId: 1337, blockGasLimit: 30000000},
          bloat: {sizeMiB: 1000, tokenIds: [0, 1, 2, 3]}
        }
    ')"
    L1_SNAPSHOT_CACHE_KEY="$(printf '%s' "$L1_SNAPSHOT_EXPECTED_CONFIG" | sha256sum | awk '{print $1}')"

    write_manifest "$L1_SNAPSHOT_A_MANIFEST" a fixture-generation
    write_manifest "$L1_SNAPSHOT_B_MANIFEST" b fixture-generation
}

mutate_manifest() {
    local manifest="$1"
    local filter="$2"
    local temporary="$manifest.tmp"
    jq "$filter" "$manifest" >"$temporary"
    mv -f -- "$temporary" "$manifest"
}

setup_valid_pair
expect_success "matching paired manifests validate" l1_snapshot_validate_cache

setup_valid_pair
mutate_manifest "$L1_SNAPSHOT_B_MANIFEST" '.generationId = "other-generation"'
expect_failure "different snapshot generations are rejected" l1_snapshot_validate_cache

setup_valid_pair
mutate_manifest "$L1_SNAPSHOT_A_MANIFEST" '.cacheKey = "other-cache-key"'
expect_failure "a mismatched cache key is rejected" l1_snapshot_validate_cache

setup_valid_pair
mutate_manifest "$L1_SNAPSHOT_B_MANIFEST" '.config.l1.chainId = 999'
expect_failure "a mismatched snapshot config is rejected" l1_snapshot_validate_cache

setup_valid_pair
jq '.alloc[$factory].code = "0xdeadbeef"' \
    --arg factory "$L1_SNAPSHOT_ZONE_FACTORY" \
    "$L1_SNAPSHOT_PATCHED_GENESIS" >"$L1_SNAPSHOT_PATCHED_GENESIS.tmp"
mv -f -- "$L1_SNAPSHOT_PATCHED_GENESIS.tmp" "$L1_SNAPSHOT_PATCHED_GENESIS"
expect_failure "a changed patched genesis is rejected" l1_snapshot_validate_cache

expect_success \
    "a narrowly named snapshot root is accepted" \
    l1_snapshot_validate_root_for_rebuild "$TEST_TMP/roots/zones-l1-fixture"
expect_failure "the filesystem root is rejected" l1_snapshot_validate_root_for_rebuild /
expect_failure \
    "a top-level snapshot root is rejected" \
    l1_snapshot_validate_root_for_rebuild /zones-l1-fixture
expect_failure \
    "a root without the snapshot prefix is rejected" \
    l1_snapshot_validate_root_for_rebuild "$TEST_TMP/roots/l1-fixture"

echo "1..$tests_run"
