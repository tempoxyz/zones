# Checker-lab inspection and log commands.

status() {
    prepare
    local l1_running=false zone_running=false
    if pid_is_running "$L1_PID_FILE"; then
        l1_running=true
        printf 'Tempo L1: running (PID %s, HTTP %s, WS %s)\n' \
            "$(cat "$L1_PID_FILE")" "$L1_HTTP_URL" "$L1_WS_URL"
    else
        printf 'Tempo L1: stopped\n'
    fi
    if pid_is_running "$ZONE_PID_FILE"; then
        zone_running=true
        printf 'Zone:     running (PID %s, HTTP %s, metrics %s)\n' \
            "$(cat "$ZONE_PID_FILE")" "$ZONE_HTTP_URL" "$ZONE_METRICS_URL"
    else
        printf 'Zone:     stopped\n'
    fi
    printf 'State:    %s\n' "$STATE_DIR"

    if [[ -d "$CHECKER_DB" && -x "$ZONE_BIN" ]]; then
        local checker_state
        if ! checker_state="$(checker_json 2>/dev/null)"; then
            printf 'Checker:  database currently unavailable for inspection\n'
            return
        fi

        local imported_tip verified_tip observed_tip recovering active_finding coverage_gap
        imported_tip="$(jq -r '.importedTempoTip.number' <<<"$checker_state")"
        verified_tip="$(jq -r '.verifiedZoneTip.number' <<<"$checker_state")"
        observed_tip="$(jq -r '.observedZoneTip.number' <<<"$checker_state")"
        recovering="$(jq -r '.recovering' <<<"$checker_state")"
        active_finding="$(jq -r '.activeFinding' <<<"$checker_state")"
        coverage_gap="$(jq -r '.hasCoverageGap' <<<"$checker_state")"

        printf '\nLive tips:\n'
        if [[ "$l1_running" == true ]]; then
            local l1_head l1_finalized
            l1_head="$(cast block-number --rpc-url "$L1_HTTP_URL")"
            l1_finalized="$(( $(cast block finalized --rpc-url "$L1_HTTP_URL" --json | jq -r '.number') ))"
            printf '  Tempo L1 head:       %s\n' "$l1_head"
            printf '  Tempo L1 finalized:  %s (head distance: %s blocks)\n' \
                "$l1_finalized" "$((l1_head - l1_finalized))"
            printf '  Imported Tempo tip:  %s (finalized lag: %s blocks)\n' \
                "$imported_tip" "$((l1_finalized - imported_tip))"
        else
            printf '  Imported Tempo tip:  %s\n' "$imported_tip"
        fi
        if [[ "$zone_running" == true ]]; then
            local zone_head
            zone_head="$(cast block-number --rpc-url "$ZONE_HTTP_URL")"
            printf '  Zone head:           %s\n' "$zone_head"
            printf '  Verified Zone tip:   %s (lag: %s blocks)\n' \
                "$verified_tip" "$((zone_head - verified_tip))"
        else
            printf '  Verified Zone tip:   %s\n' "$verified_tip"
        fi
        printf '\nChecker:\n'
        printf '  Observed Zone tip:   %s\n' "$observed_tip"
        printf '  Recovering:          %s\n' "$recovering"
        printf '  Active finding:      %s\n' "$active_finding"
        printf '  Coverage gap:        %s\n' "$coverage_gap"
        printf '\nDurable checker state:\n%s\n' "$checker_state"
    fi
}


logs() {
    prepare
    case "${1:-zone}" in
        l1) tail -f "$L1_LOG" ;;
        zone) tail -f "$ZONE_LOG" ;;
        *) die "logs expects 'l1' or 'zone'" ;;
    esac
}
