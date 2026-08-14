# Zone lifecycle and checkpoint management for the checker lab.

stop_zone() {
    stop_one "$ZONE_PID_FILE" "Zone"
}

zone_metadata() {
    local field="$1"
    jq -er ".$field" "$ZONE_DIR/zone.json" \
        || die "Zone metadata field '$field' is missing from $ZONE_DIR/zone.json"
}

provision_zone() {
    [[ ! -f "$ZONE_DIR/zone.json" ]] || return 0
    say "Provisioning the development Zone"
    env DEV_KEY="$DEV_KEY" RUST_LOG_STYLE=never "$ZONE_BIN" dev \
        --l1.rpc-url "$L1_WS_URL" \
        --datadir "$ZONE_DIR" \
        --http.addr 127.0.0.1 \
        --http.port "$ZONE_HTTP_PORT" \
        --redacted-rpc.port "$ZONE_REDACTED_PORT" \
        >"$ZONE_LOG" 2>&1 &
    echo "$!" >"$ZONE_PID_FILE"
    wait_for_rpc "$ZONE_HTTP_URL" "$ZONE_PID_FILE" "Zone provisioning node" "$ZONE_LOG"
    stop_zone
}

wait_for_checker_checkpoint() {
    local i
    for ((i = 0; i < 120; i++)); do
        assert_running "$ZONE_PID_FILE" "Zone" "$ZONE_LOG"
        [[ -d "$CHECKER_DB" ]] && return
        sleep 1
    done
    die "timed out waiting for checker checkpoint; inspect $ZONE_LOG"
}

start_zone() {
    if pid_is_running "$ZONE_PID_FILE"; then
        return
    fi
    [[ -f "$ZONE_DIR/zone.json" ]] || die "Zone is not provisioned; run 'up'"
    if cast block-number --rpc-url "$ZONE_HTTP_URL" >/dev/null 2>&1; then
        die "$ZONE_HTTP_URL is already serving RPC but is not owned by the checker lab"
    fi
    local portal zone_id sequencer_key_file
    portal="$(zone_metadata portal)"
    zone_id="$(zone_metadata zoneId)"
    sequencer_key_file="$ZONE_DIR/sequencer.key"
    [[ -f "$sequencer_key_file" ]] || die "Zone sequencer key file is missing; run 'up'"
    say "Starting Zone with checker observe mode; log: $ZONE_LOG"
    (
        cd "$ZONES_ROOT"
        export RUST_LOG="${RUST_LOG:-info,zone::checker=debug}"
        export RUST_LOG_STYLE=never
        exec "$ZONE_BIN" node \
            --chain "$ZONE_DIR/genesis.json" \
            --datadir "$ZONE_DATADIR" \
            --l1.rpc-url "$L1_WS_URL" \
            --l1.portal-address "$portal" \
            --zone.id "$zone_id" \
            --http --http.addr 127.0.0.1 --http.port "$ZONE_HTTP_PORT" --http.api all \
            --metrics "127.0.0.1:$ZONE_METRICS_PORT" \
            --redacted-rpc.port "$ZONE_REDACTED_PORT" \
            --log.file.directory "$ZONE_DIR/logs" \
            --sequencer \
            --sequencer-key-file "$sequencer_key_file" \
            --checker.mode observe \
            --checker.database-path "$CHECKER_DB"
    ) >"$ZONE_LOG" 2>&1 &
    echo "$!" >"$ZONE_PID_FILE"
    wait_for_rpc "$ZONE_HTTP_URL" "$ZONE_PID_FILE" "Zone" "$ZONE_LOG"
    wait_for_checker_checkpoint
}


up() {
    prepare
    start_l1
    prepare_l1_protocol
    build_zone
    provision_zone
    start_zone
    status
}

restart_zone() {
    prepare
    assert_running "$L1_PID_FILE" "Tempo L1" "$L1_LOG"
    stop_zone
    build_zone
    start_zone
    status
}


down() {
    prepare
    stop_zone
    stop_one "$L1_PID_FILE" "Tempo L1"
}

reset() {
    down
    say "Removing checker-lab state: $STATE_DIR"
    rm -rf "$STATE_DIR"
}
