# shellcheck shell=bash
# Checker-lab status and logs.

status() {
    prepare
    if pid_is_running "$L1_PID_FILE"; then
        printf 'Tempo:  running (PID %s, %s)\n' "$(<"$L1_PID_FILE")" "$L1_HTTP_URL"
    else printf 'Tempo:  stopped\n'; fi
    if pid_is_running "$ZONE_PID_FILE"; then
        printf 'Zone:   running (PID %s, %s)\n' "$(<"$ZONE_PID_FILE")" "$ZONE_HTTP_URL"
    else printf 'Zone:   stopped\n'; fi
    printf 'Binary: %s\nState:  %s\n' "$ZONE_BIN" "$STATE_DIR"
    if pid_is_running "$ZONE_PID_FILE" && curl -fsS "$ZONE_METRICS_URL" >/dev/null 2>&1; then
        printf '\nChecker:\n'
        printf '  observed Zone height: %s\n' "$(metric reth_tempo_zone_checker_observed_zone_height)"
        printf '  verified Zone height: %s\n' "$(metric reth_tempo_zone_checker_verified_zone_height)"
        printf '  imported Tempo height: %s\n' "$(metric reth_tempo_zone_checker_imported_tempo_height)"
        printf '  verification lag:     %s\n' "$(metric reth_tempo_zone_checker_verification_lag_blocks)"
        printf '  divergence active:    %s\n' "$(metric reth_tempo_zone_checker_divergence_active)"
        printf '  acquisition retries:  %s\n' "$(metric reth_tempo_zone_checker_acquisition_retries_total)"
    fi
}

logs() {
    prepare
    case "$1" in l1) tail -f "$L1_LOG" ;; zone) tail -f "$ZONE_LOG" ;; *) die "logs expects zone or l1" ;; esac
}
