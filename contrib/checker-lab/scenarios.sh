# shellcheck shell=bash
# Bridge scenarios and checker assertions.

export_trigger_environment() {
    export L1_RPC_URL="$L1_HTTP_URL"
    L1_PORTAL_ADDRESS="$(zone_metadata portal)"
    export L1_PORTAL_ADDRESS
    export ZONE_RPC_URL="$ZONE_HTTP_URL"
    export PRIVATE_KEY="$DEV_KEY"
    export ADMIN_KEY="$DEV_KEY"
}

wait_for_imported_tempo_block() {
    local target="$1" label="$2" imported verified divergence
    for ((i = 0; i < 180; i++)); do
        assert_running "$ZONE_PID_FILE" "Zone" "$ZONE_LOG"
        imported="$(metric reth_tempo_zone_checker_imported_tempo_height 2>/dev/null || true)"
        verified="$(metric reth_tempo_zone_checker_verified_zone_height 2>/dev/null || true)"
        divergence="$(metric reth_tempo_zone_checker_divergence_active 2>/dev/null || true)"
        if [[ "$divergence" == 1 ]]; then
            printf 'FAIL %s: checker divergence before importing Tempo block %s\n' "$label" "$target" >&2
            tail -n 30 "$ZONE_LOG" >&2
            return 1
        fi
        if [[ -n "$imported" ]] && awk -v value="$imported" -v target="$target" 'BEGIN { exit !(value >= target) }'; then
            printf 'PASS %-12s imported Tempo block %s at verified Zone block %s\n' \
                "$label" "$target" "$verified"
            return
        fi
        sleep 1
    done
    die "timed out waiting for checker to import Tempo block $target for $label"
}

trigger_one() {
    local scenario="$1"
    case "$scenario" in
        token)
            local suffix salt output token
            suffix="$(date +%s)-$$"; salt="$(cast keccak "checker-lab-$suffix")"
            output="$(just --justfile "$ZONES_ROOT/Justfile" create-token "Checker $suffix" CHK "$salt")"
            printf '%s\n' "$output"
            token="$(awk '/Address:/ {print $2; exit}' <<<"$output")"
            [[ "$token" =~ ^0x[0-9a-fA-F]{40}$ ]] || die "could not parse token address"
            just --justfile "$ZONES_ROOT/Justfile" enable-token "$token"
            ;;
        deposit)
            just --justfile "$ZONES_ROOT/Justfile" max-approve-portal "$PATH_USD"
            just --justfile "$ZONES_ROOT/Justfile" send-deposit 1000000 "$DEV_ADDRESS" \
                0x0000000000000000000000000000000000000000000000000000000000000000 \
                "$PATH_USD" "$ZONE_HTTP_URL"
            ;;
        withdrawal)
            just --justfile "$ZONES_ROOT/Justfile" max-approve-portal "$PATH_USD"
            just --justfile "$ZONES_ROOT/Justfile" send-deposit 1000000 "$DEV_ADDRESS" \
                0x0000000000000000000000000000000000000000000000000000000000000000 \
                "$PATH_USD" "$ZONE_HTTP_URL"
            just --justfile "$ZONES_ROOT/Justfile" max-approve-outbox "$PATH_USD" "$ZONE_HTTP_URL"
            just --justfile "$ZONES_ROOT/Justfile" send-withdrawal 100000 "$DEV_ADDRESS" "$PATH_USD" \
                0x0000000000000000000000000000000000000000000000000000000000000000 \
                0 "$DEV_ADDRESS" 0x 0x "$ZONE_HTTP_URL"
            ;;
        *) die "trigger expects token, deposit, withdrawal, or all" ;;
    esac
    local target; target="$(cast block-number --rpc-url "$L1_HTTP_URL")"
    wait_for_imported_tempo_block "$target" "$scenario"
}

trigger() {
    assert_running "$L1_PID_FILE" "Tempo" "$L1_LOG"
    assert_running "$ZONE_PID_FILE" "Zone" "$ZONE_LOG"
    export_trigger_environment
    if [[ "$1" == all ]]; then
        trigger_one token; trigger_one deposit; trigger_one withdrawal
    else
        trigger_one "$1"
    fi
}
