#!/usr/bin/env bash

# Provision the production-shaped local topology used by the Zones benchmark:
# two real Tempo consensus validators plus one release/profiling Zone sequencer.
#
# The caller must build the binaries first. A typical workflow uses the Tempo
# revision pinned in the Zones workspace and passes:
#
#   TEMPO_ROOT=/path/to/tempo
#   TEMPO_BIN=/path/to/tempo/target/profiling/tempo
#   TEMPO_XTASK_BIN=/path/to/tempo/target/profiling/tempo-xtask
#   ZONES_XTASK_BIN=/path/to/zones/target/profiling/tempo-xtask
#   ZONE_BIN=/path/to/zones/target/profiling/tempo-zone
#
# `prepare-l1` builds the paired L1 baseline on a cache miss but deliberately
# leaves Schelk promotion to the caller. `up` starts from a verified private
# restored copy and stops after node and contract readiness. It does not fund
# Zone accounts, submit benchmark deposits, or wait for benchmark bridge events.
# On success the processes remain alive for a following run-phase.sh invocation.
# Always call `cleanup` from the workflow's `if: always()` teardown step.

set -Eeuo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly ZONES_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
readonly ZONE_FACTORY="0x5aF2000000000000000000000000000000000000"
readonly PATH_USD="0x20C0000000000000000000000000000000000000"
readonly DLUSD="0x20C0000000000000000000000000000000000001"
readonly TEMPO_STATE="0x1c00000000000000000000000000000000000000"
readonly ZONE_CONFIG="0x1c00000000000000000000000000000000000003"
readonly EIP2935_HISTORY_STORAGE="0x0000F90827F1C53a10cb7A02335B175320002935"
readonly LOCALNET_SIGNING_SECRET="tempo-localnet-signing-key-secret"

provision_succeeded=0
provision_pid_file=""

die() {
    echo "error: $*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

require_file() {
    [[ -f "$1" ]] || die "required file not found: $1"
}

require_dir() {
    [[ -d "$1" ]] || die "required directory not found: $1"
}

require_executable() {
    [[ -x "$1" ]] || die "required executable not found: $1"
}

require_uint() {
    local name="$1"
    local value="${!name:-}"
    [[ "$value" =~ ^[0-9]+$ ]] || die "$name must be an unsigned integer"
}

rpc() {
    local url="$1"
    local method="$2"
    local params="${3:-[]}" response
    response="$(curl --silent --show-error --fail \
        --header 'Content-Type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}" \
        "$url")" || return 1
    jq -e '.error == null' >/dev/null <<<"$response" || return 1
    jq -r '.result' <<<"$response"
}

hex_to_dec() {
    local value="$1"
    [[ "$value" =~ ^0x[0-9a-fA-F]+$ ]] || return 1
    printf '%d\n' "$((16#${value#0x}))"
}

wait_for_rpc() {
    local url="$1"
    local label="$2"
    local timeout="$3"
    local deadline=$((SECONDS + timeout))
    while (( SECONDS < deadline )); do
        if rpc "$url" eth_blockNumber >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    die "timed out waiting for $label RPC at $url"
}

wait_for_peer() {
    local url="$1"
    local label="$2"
    local timeout="$3"
    local deadline=$((SECONDS + timeout)) peer_hex peer_count
    while (( SECONDS < deadline )); do
        peer_hex="$(rpc "$url" net_peerCount 2>/dev/null || true)"
        peer_count="$(hex_to_dec "$peer_hex" 2>/dev/null || true)"
        if [[ -n "$peer_count" ]] && (( peer_count >= 1 )); then
            return 0
        fi
        sleep 1
    done
    die "timed out waiting for $label to connect to its Tempo peer"
}

wait_for_chain_advance() {
    local url="$1"
    local label="$2"
    local timeout="$3"
    local start_hex start_block current_hex current_block
    local deadline=$((SECONDS + timeout))

    start_hex="$(rpc "$url" eth_blockNumber)"
    start_block="$(hex_to_dec "$start_hex")"
    while (( SECONDS < deadline )); do
        current_hex="$(rpc "$url" eth_blockNumber 2>/dev/null || true)"
        current_block="$(hex_to_dec "$current_hex" 2>/dev/null || true)"
        if [[ -n "$current_block" ]] && (( current_block > start_block )); then
            return 0
        fi
        sleep 1
    done
    die "timed out waiting for $label chain to advance past block $start_block"
}

verify_history_storage() {
    local url="$1"
    local label="$2"
    local code current_hex current_block probe_block probe_hex calldata expected observed

    code="$(rpc "$url" eth_getCode "[\"$EIP2935_HISTORY_STORAGE\",\"latest\"]")"
    [[ "$code" != "0x" ]] \
        || die "$label is missing the EIP-2935 history storage contract"

    current_hex="$(rpc "$url" eth_blockNumber)"
    current_block="$(hex_to_dec "$current_hex")"
    (( current_block > 0 )) \
        || die "$label has not advanced far enough to verify EIP-2935 history"
    probe_block=$((current_block - 1))
    printf -v probe_hex '0x%x' "$probe_block"
    printf -v calldata '0x%064x' "$probe_block"
    expected="$(rpc "$url" eth_getBlockByNumber "[\"$probe_hex\",false]" | jq -er '.hash')"
    observed="$(rpc "$url" eth_call \
        "[{\"to\":\"$EIP2935_HISTORY_STORAGE\",\"data\":\"$calldata\"},\"latest\"]")"
    [[ "${observed,,}" == "${expected,,}" ]] \
        || die "$label EIP-2935 history mismatch for block $probe_block: expected $expected, got $observed"
}

process_matches() {
    local pid="$1"
    local expected="$2"
    [[ -r "/proc/$pid/cmdline" ]] || return 1
    tr '\0' ' ' <"/proc/$pid/cmdline" | grep -F -- "$expected" >/dev/null
}

cleanup_pid_file() {
    local pid_file="$1"
    [[ -f "$pid_file" ]] || return 0

    local -a records=()
    mapfile -t records <"$pid_file"

    local index name pid expected
    for ((index = ${#records[@]} - 1; index >= 0; index--)); do
        read -r name pid expected <<<"${records[index]}"
        [[ "$pid" =~ ^[0-9]+$ ]] || continue
        if kill -0 "$pid" 2>/dev/null; then
            if ! process_matches "$pid" "$expected"; then
                echo "warning: refusing to signal reused PID $pid for $name" >&2
                continue
            fi
            echo "stopping $name (PID $pid)"
            kill -INT "$pid" 2>/dev/null || true
        fi
    done

    local deadline=$((SECONDS + 30)) any_running
    while (( SECONDS < deadline )); do
        any_running=0
        for record in "${records[@]}"; do
            read -r name pid expected <<<"$record"
            if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null \
                && process_matches "$pid" "$expected"; then
                any_running=1
                break
            fi
        done
        (( any_running == 0 )) && break
        sleep 1
    done

    for ((index = ${#records[@]} - 1; index >= 0; index--)); do
        read -r name pid expected <<<"${records[index]}"
        if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null \
            && process_matches "$pid" "$expected"; then
            echo "forcing $name to stop (PID $pid)" >&2
            kill -KILL "$pid" 2>/dev/null || true
        fi
    done

    rm -f -- "$pid_file"
}

stop_stale_listener() {
    local port="$1"
    local expected="$2"
    local label="$3"
    local -a pids=()
    local pid deadline

    mapfile -t pids < <(lsof -nP -t -iTCP:"$port" -sTCP:LISTEN 2>/dev/null | sort -u || true)
    (( ${#pids[@]} == 0 )) && return 0

    for pid in "${pids[@]}"; do
        [[ "$pid" =~ ^[0-9]+$ ]] || continue
        process_matches "$pid" "$expected" ||
            die "TCP port $port is owned by PID $pid, not a stale $label process; refusing to signal it"
        echo "stopping stale $label listener on TCP port $port (PID $pid)" >&2
        kill -INT "$pid" 2>/dev/null || true
    done

    deadline=$((SECONDS + 30))
    while (( SECONDS < deadline )); do
        for pid in "${pids[@]}"; do
            if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null \
                && process_matches "$pid" "$expected"; then
                sleep 1
                continue 2
            fi
        done
        return 0
    done

    for pid in "${pids[@]}"; do
        if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null \
            && process_matches "$pid" "$expected"; then
            echo "forcing stale $label listener to stop on TCP port $port (PID $pid)" >&2
            kill -KILL "$pid" 2>/dev/null || true
        fi
    done
}

provision_on_exit() {
    local status=$?
    trap - EXIT INT TERM
    if (( provision_succeeded == 0 )) && [[ -n "$provision_pid_file" ]]; then
        cleanup_pid_file "$provision_pid_file"
    fi
    exit "$status"
}

start_process() {
    local name="$1"
    local expected="$2"
    local cpu_set="$3"
    local log_file="$4"
    shift 4

    local -a command=("$@")
    if [[ -n "$cpu_set" ]]; then
        command=(taskset --cpu-list "$cpu_set" "${command[@]}")
    fi

    "${command[@]}" >"$log_file" 2>&1 &
    local pid=$!
    printf '%s %s %s\n' "$name" "$pid" "$expected" >>"$pid_file"

    sleep 2
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "$name exited during startup; log follows:" >&2
        tail -n 200 "$log_file" >&2 || true
        return 1
    fi
}

derive_key() {
    local mnemonic_file="$1"
    local index="$2"
    cast wallet private-key --mnemonic "$mnemonic_file" --mnemonic-index "$index"
}

derive_address() {
    local mnemonic_file="$1"
    local index="$2"
    cast wallet address --mnemonic "$mnemonic_file" --mnemonic-index "$index"
}

wait_for_zone_configuration() {
    local zone_rpc="$1"
    local anchor_block="$2"
    local expected_sequencer="$3"
    local initial_token="$4"
    local timeout="$5"
    local deadline=$((SECONDS + timeout)) finalized is_sequencer enabled

    while (( SECONDS < deadline )); do
        finalized="$(cast call "$TEMPO_STATE" 'tempoBlockNumber()(uint64)' \
            --rpc-url "$zone_rpc" 2>/dev/null | awk '{print $1}' || true)"
        is_sequencer="$(cast call "$ZONE_CONFIG" 'isSequencer(address)(bool)' "$expected_sequencer" \
            --rpc-url "$zone_rpc" 2>/dev/null | awk '{print $1}' || true)"
        enabled="$(cast call "$ZONE_CONFIG" 'isEnabledToken(address)(bool)' "$initial_token" \
            --rpc-url "$zone_rpc" 2>/dev/null | awk '{print $1}' || true)"

        if [[ "$finalized" =~ ^[0-9]+$ ]] \
            && (( finalized > anchor_block )) \
            && [[ "$is_sequencer" == "true" ]] \
            && [[ "$enabled" == "true" ]]; then
            return 0
        fi
        sleep 1
    done

    die "timed out waiting for the Zone to ingest its portal configuration past L1 anchor \
$anchor_block (finalized=${finalized:-unavailable}, \
expected_sequencer=${expected_sequencer}, is_sequencer=${is_sequencer:-unavailable}, \
token=${initial_token}, enabled=${enabled:-unavailable})"
}

wait_for_zone_enabled_token() {
    local zone_rpc="$1"
    local token="$2"
    local timeout="$3"
    local deadline=$((SECONDS + timeout)) enabled
    while (( SECONDS < deadline )); do
        enabled="$(cast call "$ZONE_CONFIG" 'isEnabledToken(address)(bool)' "$token" \
            --rpc-url "$zone_rpc" 2>/dev/null | awk '{print $1}' || true)"
        [[ "$enabled" == "true" ]] && return 0
        sleep 1
    done
    die "timed out waiting for Zone to enable token $token"
}

write_env() {
    local env_file="$1"
    shift
    mkdir -p -- "$(dirname -- "$env_file")"
    : >"$env_file"
    chmod 600 "$env_file"
    while (( $# > 0 )); do
        local name="$1"
        local value="$2"
        shift 2
        printf 'export %s=%q\n' "$name" "$value" >>"$env_file"
    done
}

provision_up() {
    require_command awk
    require_command cast
    require_command curl
    require_command jq
    require_command lsof
    require_command taskset

    [[ -n "${ZONES_BENCH_MNEMONIC_FILE:-}" ]] || die "ZONES_BENCH_MNEMONIC_FILE must be set"
    require_file "$ZONES_BENCH_MNEMONIC_FILE"
    [[ -n "${TEMPO_ROOT:-}" ]] || die "TEMPO_ROOT must be set"

    TEMPO_BIN="${TEMPO_BIN:-$TEMPO_ROOT/target/profiling/tempo}"
    TEMPO_XTASK_BIN="${TEMPO_XTASK_BIN:-$TEMPO_ROOT/target/profiling/tempo-xtask}"
    ZONES_XTASK_BIN="${ZONES_XTASK_BIN:-$ZONES_ROOT/target/profiling/tempo-xtask}"
    ZONE_BIN="${ZONE_BIN:-$ZONES_ROOT/target/profiling/tempo-zone}"
    require_executable "$TEMPO_BIN"
    require_executable "$TEMPO_XTASK_BIN"
    require_executable "$ZONES_XTASK_BIN"
    require_executable "$ZONE_BIN"

    # A cancelled workflow can prevent its checkout-scoped teardown from
    # running. Only reclaim listeners owned by the exact benchmark binaries;
    # fail rather than signal an unrelated process.
    stop_stale_listener 8545 "$TEMPO_BIN" "Tempo validator A"
    stop_stale_listener 8645 "$TEMPO_BIN" "Tempo validator B"
    stop_stale_listener 8546 "$ZONE_BIN" "Zone"

    local account_start="${ZONES_BENCH_ACCOUNT_START:-16}"
    local accounts="${ZONES_BENCH_ACCOUNTS:-200}"
    local account_capacity="${ZONES_BENCH_ACCOUNT_CAPACITY:-10000}"
    local l1_chain_id="${ZONES_BENCH_L1_CHAIN_ID:-1337}"
    local l1_gas_limit="${ZONES_BENCH_L1_GAS_LIMIT:-30000000}"
    local l1_general_gas_limit="${ZONES_BENCH_L1_GENERAL_GAS_LIMIT:-$l1_gas_limit}"
    local bloat_mib="${ZONES_BENCH_BLOAT_MIB:-1024}"
    local bloat_balance="${ZONES_BENCH_BLOAT_BALANCE:-18446744073709551615}"
    local rpc_timeout="${ZONES_BENCH_RPC_TIMEOUT_SECS:-300}"
    local zone_timeout="${ZONES_BENCH_ZONE_TIMEOUT_SECS:-300}"
    local run_key="${GITHUB_RUN_ID:-local}-${GITHUB_RUN_ATTEMPT:-1}"
    local profile="${ZONES_BENCH_PROFILE:-generic}"
    case "$profile" in
        generic|neobank) ;;
        *) die "ZONES_BENCH_PROFILE must be generic or neobank" ;;
    esac

    ZONES_BENCH_ACCOUNT_START="$account_start"
    ZONES_BENCH_ACCOUNTS="$accounts"
    ZONES_BENCH_ACCOUNT_CAPACITY="$account_capacity"
    ZONES_BENCH_L1_CHAIN_ID="$l1_chain_id"
    ZONES_BENCH_L1_GAS_LIMIT="$l1_gas_limit"
    ZONES_BENCH_L1_GENERAL_GAS_LIMIT="$l1_general_gas_limit"
    ZONES_BENCH_BLOAT_MIB="$bloat_mib"
    ZONES_BENCH_BLOAT_BALANCE="$bloat_balance"
    ZONES_BENCH_RPC_TIMEOUT_SECS="$rpc_timeout"
    ZONES_BENCH_ZONE_TIMEOUT_SECS="$zone_timeout"
    for name in \
        ZONES_BENCH_ACCOUNT_START ZONES_BENCH_ACCOUNTS ZONES_BENCH_ACCOUNT_CAPACITY \
        ZONES_BENCH_L1_CHAIN_ID \
        ZONES_BENCH_L1_GAS_LIMIT ZONES_BENCH_L1_GENERAL_GAS_LIMIT \
        ZONES_BENCH_BLOAT_MIB ZONES_BENCH_BLOAT_BALANCE \
        ZONES_BENCH_RPC_TIMEOUT_SECS ZONES_BENCH_ZONE_TIMEOUT_SECS
    do
        require_uint "$name"
    done
    account_start=$((10#$account_start))
    accounts=$((10#$accounts))
    account_capacity=$((10#$account_capacity))
    l1_chain_id=$((10#$l1_chain_id))
    l1_gas_limit=$((10#$l1_gas_limit))
    l1_general_gas_limit=$((10#$l1_general_gas_limit))
    bloat_mib=$((10#$bloat_mib))
    rpc_timeout=$((10#$rpc_timeout))
    zone_timeout=$((10#$zone_timeout))
    (( accounts > 0 )) || die "ZONES_BENCH_ACCOUNTS must be greater than zero"
    (( accounts <= account_capacity )) \
        || die "ZONES_BENCH_ACCOUNTS exceeds the cached funded-account capacity"
    (( account_start >= 5 )) || die "ZONES_BENCH_ACCOUNT_START must be at least 5; indices 0-4 are reserved for the factory owner, two validator identities, portal admin, and sequencer"
    (( l1_chain_id > 0 )) || die "ZONES_BENCH_L1_CHAIN_ID must be greater than zero"
    (( l1_gas_limit > 0 )) || die "ZONES_BENCH_L1_GAS_LIMIT must be greater than zero"
    (( l1_general_gas_limit > 0 )) \
        || die "ZONES_BENCH_L1_GENERAL_GAS_LIMIT must be greater than zero"
    (( rpc_timeout > 0 )) || die "ZONES_BENCH_RPC_TIMEOUT_SECS must be greater than zero"
    (( zone_timeout > 0 )) || die "ZONES_BENCH_ZONE_TIMEOUT_SECS must be greater than zero"

    export ZONES_BENCH_ACCOUNT_START ZONES_BENCH_ACCOUNTS ZONES_BENCH_ACCOUNT_CAPACITY
    export ZONES_BENCH_L1_CHAIN_ID ZONES_BENCH_L1_GAS_LIMIT ZONES_BENCH_L1_GENERAL_GAS_LIMIT
    export ZONES_BENCH_BLOAT_MIB ZONES_BENCH_BLOAT_BALANCE

    "$SCRIPT_DIR/l1-snapshot.sh" verify

    local control_root="${ZONES_BENCH_TOPOLOGY_DIR:-}"
    if [[ -z "$control_root" ]]; then
        control_root="$(mktemp -d "${RUNNER_TEMP:-/tmp}/zones-benchmark-topology.XXXXXX")"
    else
        [[ ! -e "$control_root" ]] || die "ZONES_BENCH_TOPOLOGY_DIR already exists: $control_root"
        mkdir -p "$control_root"
    fi

    local state_a_root="${ZONES_BENCH_STATE_A_ROOT:-/reth-bench-a/zones-l1-${bloat_mib}mb}"
    local state_b_root="${ZONES_BENCH_STATE_B_ROOT:-/reth-bench-b/zones-l1-${bloat_mib}mb}"
    [[ -d "$state_a_root" ]] || die "validator A L1 snapshot is missing: $state_a_root"
    [[ -d "$state_b_root" ]] || die "validator B L1 snapshot is missing: $state_b_root"
    local zone_state_root="${ZONES_BENCH_ZONE_STATE_ROOT:-/reth-bench-a/zones-runtime-$run_key}"
    [[ ! -e "$zone_state_root" ]] || die "Zone runtime state path already exists: $zone_state_root"
    mkdir -p "$zone_state_root"

    local localnet_dir="$state_a_root/localnet"
    local patched_genesis="$state_a_root/tempo-genesis.json"
    local l1_a_db="$state_a_root/l1-a"
    local l1_b_db="$state_b_root/l1-b"
    local zone_db="$zone_state_root/zone"
    local zone_dir="$control_root/zone"
    local log_dir="$control_root/logs"
    local env_file="${ZONES_BENCH_ENV_FILE:-$control_root/topology.env}"
    pid_file="${ZONES_BENCH_PID_FILE:-$control_root/topology.pids}"
    mkdir -p "$log_dir" "$(dirname -- "$pid_file")"
    : >"$pid_file"
    chmod 600 "$pid_file"

    provision_succeeded=0
    provision_pid_file="$pid_file"
    trap provision_on_exit EXIT
    trap 'exit 130' INT TERM

    local mnemonic_file="$ZONES_BENCH_MNEMONIC_FILE"
    local owner_key sequencer_key admin_key owner_address admin_address sequencer_address
    owner_key="$(derive_key "$mnemonic_file" 0)"
    admin_key="$(derive_key "$mnemonic_file" 3)"
    sequencer_key="$(derive_key "$mnemonic_file" 4)"
    owner_address="$(derive_address "$mnemonic_file" 0)"
    admin_address="$(derive_address "$mnemonic_file" 3)"
    sequencer_address="$(derive_address "$mnemonic_file" 4)"

    mkdir -p "$zone_db" "$zone_dir"

    local validator_a="$localnet_dir/127.0.0.2:8000"
    local validator_b="$localnet_dir/127.0.0.3:8100"
    for path in \
        "$validator_a/signing.key" "$validator_a/signing.share" "$validator_a/enode.key" "$validator_a/enode.identity" \
        "$validator_b/signing.key" "$validator_b/signing.share" "$validator_b/enode.key" "$validator_b/enode.identity"
    do
        require_file "$path"
    done
    local trusted_peers
    trusted_peers="enode://$(tr -d '[:space:]' <"$validator_a/enode.identity")@127.0.0.2:8001,enode://$(tr -d '[:space:]' <"$validator_b/enode.identity")@127.0.0.3:8101"
    local consensus_secret="$control_root/consensus-secret"
    (umask 077; printf '%s\n' "$LOCALNET_SIGNING_SECRET" >"$consensus_secret")

    local -a common_l1_args=(
        --ipcdisable
        --disable-discovery
        --trusted-only
        --tempo.bootnodes-endpoint none
        --consensus.no-legacy-archive
        --engine.share-execution-cache-with-payload-builder
        --builder.enable-prewarming
        --builder.gaslimit "$l1_gas_limit"
        --rpc.max-connections 10000
        --txpool.pending-max-count 200000
        --txpool.basefee-max-count 200000
        --txpool.queued-max-count 200000
        --txpool.max-pending-txns 200000
        --txpool.max-new-txns 200000
        --txpool.max-batch-size 200000
    )

    start_process tempo-a "$TEMPO_BIN" "${ZONES_BENCH_L1_A_CPUS:-0-3,16-19}" "$log_dir/tempo-a.log" \
        "$TEMPO_BIN" node \
        --chain "$patched_genesis" --datadir "$l1_a_db" \
        --http --http.addr 127.0.0.1 --http.port 8545 --http.api all \
        --ws --ws.addr 127.0.0.1 --ws.port 8545 --ws.api all \
        --metrics 127.0.0.1:9001 \
        --log.file.directory "$log_dir/tempo-a" \
        --consensus.signing-key "$validator_a/signing.key" \
        --consensus.secret "$consensus_secret" \
        --consensus.signing-share "$validator_a/signing.share" \
        --consensus.listen-address 127.0.0.2:8000 \
        --consensus.metrics-address 127.0.0.2:8002 \
        --trusted-peers "$trusted_peers" \
        --port 8001 --discovery.port 8001 --discovery.v5.port 8004 \
        --p2p-secret-key "$validator_a/enode.key" --authrpc.port 8003 \
        --consensus.use-local-defaults --consensus.bypass-ip-check \
        "${common_l1_args[@]}"

    start_process tempo-b "$TEMPO_BIN" "${ZONES_BENCH_L1_B_CPUS:-4-7,20-23}" "$log_dir/tempo-b.log" \
        "$TEMPO_BIN" node \
        --chain "$patched_genesis" --datadir "$l1_b_db" \
        --http --http.addr 127.0.0.1 --http.port 8645 --http.api all \
        --ws --ws.addr 127.0.0.1 --ws.port 8645 --ws.api all \
        --metrics 127.0.0.1:9101 \
        --log.file.directory "$log_dir/tempo-b" \
        --consensus.signing-key "$validator_b/signing.key" \
        --consensus.secret "$consensus_secret" \
        --consensus.signing-share "$validator_b/signing.share" \
        --consensus.listen-address 127.0.0.3:8100 \
        --consensus.metrics-address 127.0.0.3:8102 \
        --trusted-peers "$trusted_peers" \
        --port 8101 --discovery.port 8101 --discovery.v5.port 8104 \
        --p2p-secret-key "$validator_b/enode.key" --authrpc.port 8103 \
        --consensus.use-local-defaults --consensus.bypass-ip-check \
        "${common_l1_args[@]}"

    local l1_a_rpc="http://127.0.0.1:8545"
    local l1_b_rpc="http://127.0.0.1:8645"
    wait_for_rpc "$l1_a_rpc" "Tempo validator A" "$rpc_timeout"
    wait_for_rpc "$l1_b_rpc" "Tempo validator B" "$rpc_timeout"
    wait_for_peer "$l1_a_rpc" "Tempo validator A" "$rpc_timeout"
    wait_for_peer "$l1_b_rpc" "Tempo validator B" "$rpc_timeout"
    wait_for_chain_advance "$l1_a_rpc" "Tempo validator A" "$rpc_timeout"
    wait_for_chain_advance "$l1_b_rpc" "Tempo validator B" "$rpc_timeout"
    verify_history_storage "$l1_a_rpc" "Tempo validator A"
    verify_history_storage "$l1_b_rpc" "Tempo validator B"

    local chain_a chain_b genesis_a genesis_b
    chain_a="$(hex_to_dec "$(rpc "$l1_a_rpc" eth_chainId)")"
    chain_b="$(hex_to_dec "$(rpc "$l1_b_rpc" eth_chainId)")"
    [[ "$chain_a" == "$chain_b" ]] || die "Tempo validators returned different chain IDs: $chain_a and $chain_b"
    [[ "$chain_a" == "$l1_chain_id" ]] || die "Tempo RPC chain ID $chain_a does not match generated chain ID $l1_chain_id"
    genesis_a="$(rpc "$l1_a_rpc" eth_getBlockByNumber '["0x0",false]' | jq -r '.hash')"
    genesis_b="$(rpc "$l1_b_rpc" eth_getBlockByNumber '["0x0",false]' | jq -r '.hash')"
    [[ -n "$genesis_a" && "$genesis_a" == "$genesis_b" ]] || die "Tempo validators returned different genesis hashes"
    [[ "$(rpc "$l1_a_rpc" eth_getCode "[\"$ZONE_FACTORY\",\"latest\"]")" != "0x" ]] \
        || die "canonical ZoneFactory has no code on Tempo L1"

    local zone_token="$PATH_USD"
    if [[ "$profile" == "neobank" ]]; then
        zone_token="$DLUSD"
    fi

    echo "creating a Zone through the canonical factory"
    ZONE_FACTORY_OWNER_KEY="$owner_key" "$ZONES_XTASK_BIN" create-zone \
        --output "$zone_dir" \
        --l1-rpc-url "$l1_a_rpc" \
        --zone-factory "$ZONE_FACTORY" \
        --initial-token "$zone_token" \
        --admin "$admin_address" \
        --sequencer "$sequencer_address"

    local zone_json="$zone_dir/zone.json"
    local zone_genesis="$zone_dir/genesis.json"
    require_file "$zone_json"
    require_file "$zone_genesis"
    local portal zone_id zone_chain_id anchor_block
    portal="$(jq -er '.portal' "$zone_json")"
    zone_id="$(jq -er '.zoneId' "$zone_json")"
    zone_chain_id="$(jq -er '.chainId' "$zone_json")"
    anchor_block="$(jq -er '.tempoAnchorBlock' "$zone_json")"
    [[ "$(rpc "$l1_a_rpc" eth_getCode "[\"$portal\",\"latest\"]")" != "0x" ]] \
        || die "created ZonePortal has no code on Tempo L1"

    echo "registering the sequencer encryption key"
    L1_RPC_URL="$l1_a_rpc" L1_PORTAL_ADDRESS="$portal" PRIVATE_KEY="$sequencer_key" \
        "$ZONES_XTASK_BIN" set-encryption-key

    local fixture_metadata=""
    if [[ "$profile" == "neobank" ]]; then
        local earn_artifacts="${ZONES_BENCH_EARN_ARTIFACTS:-}"
        local earn_localnet_artifacts="${ZONES_BENCH_EARN_LOCALNET_ARTIFACTS:-}"
        [[ -n "$earn_artifacts" ]] || die "ZONES_BENCH_EARN_ARTIFACTS must point to pinned Earn artifacts"
        [[ -n "$earn_localnet_artifacts" ]] || die "ZONES_BENCH_EARN_LOCALNET_ARTIFACTS must point to pinned Earn localnet artifacts"
        require_dir "$earn_artifacts"
        require_dir "$earn_localnet_artifacts"
        fixture_metadata="$control_root/neobank-fixtures.json"
        echo "deploying and configuring private-Zone benchmark fixtures"
        FIXTURE_DEPLOYER_KEY="$owner_key" PORTAL_ADMIN_KEY="$admin_key" \
            "$ZONES_XTASK_BIN" deploy-neobank-fixtures \
                --l1-rpc-url "$l1_a_rpc" \
                --portal "$portal" \
                --dlusd "$DLUSD" \
                --pathusd "$PATH_USD" \
                --earn-artifacts "$earn_artifacts" \
                --earn-localnet-artifacts "$earn_localnet_artifacts" \
                --output "$fixture_metadata"
        require_file "$fixture_metadata"
        echo "configuring zero user bridge and withdrawal protocol fees"
        SEQUENCER_KEY="$sequencer_key" "$ZONES_XTASK_BIN" configure-benchmark-fees \
            --l1-rpc-url "$l1_a_rpc" \
            --portal "$portal" \
            --token "$DLUSD" \
            --zone-gas-rate 0 \
            --bounceback-gas 0
    else
        echo "configuring non-zero deposit and bounce-back fee rates"
        SEQUENCER_KEY="$sequencer_key" "$ZONES_XTASK_BIN" configure-benchmark-fees \
            --l1-rpc-url "$l1_a_rpc" \
            --portal "$portal" \
            --token "$PATH_USD"
    fi

    export SEQUENCER_KEY="$sequencer_key"
    # The pinned Reth revision predates the retained-branch pruning fix. Its
    # default sparse-trie pruning can make a multi-transaction Zone payload
    # disagree with the root obtained during final validation.
    start_process zone "$ZONE_BIN" "${ZONES_BENCH_ZONE_CPUS:-8-13,24-29}" "$log_dir/zone.log" \
        "$ZONE_BIN" node \
        --chain "$zone_genesis" --datadir "$zone_db" \
        --l1.rpc-url ws://127.0.0.1:8545 \
        --l1.portal-address "$portal" \
        --l1.genesis-block-number "$anchor_block" \
        --zone.id "$zone_id" \
        --http --http.addr 127.0.0.1 --http.port 8546 \
        --http.api eth,net,web3,txpool \
        --metrics 127.0.0.1:9201 \
        --private-rpc.port 8544 \
        --log.file.directory "$log_dir/zone" \
        --ipcdisable \
        --engine.disable-sparse-trie-cache-pruning \
        --sequencer
    unset SEQUENCER_KEY sequencer_key owner_key admin_key

    local zone_rpc="http://127.0.0.1:8546"
    local zone_private_rpc="http://127.0.0.1:8544"
    wait_for_rpc "$zone_rpc" "Zone" "$zone_timeout"
    wait_for_chain_advance "$zone_rpc" "Zone" "$zone_timeout"
    wait_for_zone_configuration "$zone_rpc" "$anchor_block" "$sequencer_address" "$zone_token" "$zone_timeout"
    if [[ "$profile" == "neobank" ]]; then
        wait_for_zone_enabled_token "$zone_rpc" "$(jq -er '.earnToken' "$fixture_metadata")" "$zone_timeout"
    fi
    local queried_zone_chain_id
    queried_zone_chain_id="$(hex_to_dec "$(rpc "$zone_rpc" eth_chainId)")"
    [[ "$queried_zone_chain_id" == "$zone_chain_id" ]] \
        || die "Zone RPC chain ID $queried_zone_chain_id does not match zone.json chain ID $zone_chain_id"

    local target_id="local-consensus-${genesis_a#0x}-zone-$zone_id"
    local -a env_pairs=(
        L1_RPC_URL "$l1_a_rpc" \
        L1_WS_RPC_URL "ws://127.0.0.1:8545" \
        ZONES_BENCH_L1_B_RPC_URL "$l1_b_rpc" \
        ZONES_BENCH_L1_QUERY_RPC_URL "$l1_b_rpc" \
        ZONES_BENCH_L1_SUBMIT_RPC_URLS "$l1_a_rpc,$l1_b_rpc" \
        ZONE_RPC_URL "$zone_rpc" \
        ZONE_PRIVATE_RPC_URL "$zone_private_rpc" \
        ZONES_BENCH_TOKEN "$zone_token" \
        ZONES_BENCH_PROFILE "$profile" \
        L1_PORTAL_ADDRESS "$portal" \
        ZONES_BENCH_EXPECTED_L1_CHAIN_ID "$chain_a" \
        ZONES_BENCH_EXPECTED_ZONE_CHAIN_ID "$queried_zone_chain_id" \
        ZONES_BENCH_EXPECTED_ZONE_ID "$zone_id" \
        ZONES_BENCH_L1_GAS_LIMIT "$l1_gas_limit" \
        ZONES_BENCH_L1_GENERAL_GAS_LIMIT "$l1_general_gas_limit" \
        ZONES_BENCH_TARGET_ID "$target_id" \
        ZONES_BENCH_ACCOUNT_START "$account_start" \
        ZONES_BENCH_ACCOUNTS "$accounts" \
        ZONES_BENCH_L1_METRICS_URL "a:http://127.0.0.1:9001/metrics,b:http://127.0.0.1:9101/metrics" \
        ZONES_BENCH_ZONE_METRICS_URL "http://127.0.0.1:9201/metrics" \
        ZONES_BENCH_PID_FILE "$pid_file" \
        ZONES_BENCH_TOPOLOGY_DIR "$control_root" \
        ZONES_BENCH_STATE_A_ROOT "$state_a_root" \
        ZONES_BENCH_STATE_B_ROOT "$state_b_root" \
        ZONES_BENCH_ZONE_STATE_ROOT "$zone_state_root"
    )
    if [[ "$profile" == "neobank" ]]; then
        env_pairs+=(
            ZONES_BENCH_DLUSD "$DLUSD"
            ZONES_BENCH_PATHUSD "$PATH_USD"
            ZONES_BENCH_EARN_TOKEN "$(jq -er '.earnToken' "$fixture_metadata")"
            ZONES_BENCH_GATEWAY "$(jq -er '.gateway' "$fixture_metadata")"
            ZONES_BENCH_BRIDGE_WALLET "$(jq -er '.bridgeWallet' "$fixture_metadata")"
            ZONES_BENCH_DIRECT_SWAP "$(jq -er '.directSwap' "$fixture_metadata")"
            ZONES_BENCH_VAULT_ADAPTER "$(jq -er '.vaultAdapter' "$fixture_metadata")"
            ZONES_BENCH_FIXTURE_METADATA "$fixture_metadata"
        )
    fi
    write_env "$env_file" "${env_pairs[@]}"

    provision_succeeded=1
    provision_pid_file=""
    trap - EXIT INT TERM
    echo "topology ready; source $env_file before invoking a benchmark runner"
    echo "$env_file"
}

usage() {
    cat <<'EOF'
Usage:
  contrib/bench/provision-topology.sh prepare-l1
  contrib/bench/provision-topology.sh verify-l1
  contrib/bench/provision-topology.sh up
  contrib/bench/provision-topology.sh cleanup [PID_FILE]

`prepare-l1` builds both L1 baselines on the restored Schelk scratch volumes;
the caller promotes and restores them before `verify-l1`. `up` leaves the
provisioned nodes running and prints the generated non-secret
environment file path. `cleanup` stops exactly the processes recorded in the
PID file; pass it explicitly or set ZONES_BENCH_PID_FILE.
EOF
}

case "${1:-}" in
    prepare-l1)
        "$SCRIPT_DIR/l1-snapshot.sh" prepare
        ;;
    verify-l1)
        "$SCRIPT_DIR/l1-snapshot.sh" verify
        ;;
    up)
        provision_up
        ;;
    cleanup)
        cleanup_target="${2:-${ZONES_BENCH_PID_FILE:-}}"
        [[ -n "$cleanup_target" ]] || die "cleanup requires a PID file argument or ZONES_BENCH_PID_FILE"
        cleanup_pid_file "$cleanup_target"
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
