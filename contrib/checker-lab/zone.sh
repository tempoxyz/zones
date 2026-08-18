# shellcheck shell=bash
# Zone lifecycle for the checker lab.

stop_zone() { stop_one "$ZONE_PID_FILE" "Zone"; }
zone_metadata() { jq -er ".$1" "$ZONE_DIR/zone.json" || die "missing Zone metadata field $1"; }

provision_zone() {
    [[ -f "$ZONE_DIR/zone.json" ]] && return
    say "Provisioning development Zone"
    env DEV_KEY="$DEV_KEY" RUST_LOG_STYLE=never "$ZONE_BIN" dev \
        --l1.rpc-url "$L1_WS_URL" --datadir "$ZONE_DIR" \
        --http.addr 127.0.0.1 --http.port "$ZONE_HTTP_PORT" \
        --redacted-rpc.port "$ZONE_REDACTED_PORT" >"$ZONE_LOG" 2>&1 &
    echo "$!" >"$ZONE_PID_FILE"
    wait_for_rpc "$ZONE_HTTP_URL" "$ZONE_PID_FILE" "Zone provisioning node" "$ZONE_LOG"
    stop_zone
}

start_zone() {
    pid_is_running "$ZONE_PID_FILE" && return
    [[ -f "$ZONE_DIR/zone.json" ]] || die "Zone is not provisioned"
    cast block-number --rpc-url "$ZONE_HTTP_URL" >/dev/null 2>&1 \
        && die "$ZONE_HTTP_URL is occupied by a process not owned by the lab"
    local portal zone_id key
    portal="$(zone_metadata portal)"; zone_id="$(zone_metadata zoneId)"
    key="$ZONE_DIR/sequencer.key"
    [[ -x "$ZONE_BIN" ]] || die "Zone binary not found: $ZONE_BIN"
    [[ -f "$key" ]] || die "Zone sequencer key is missing"
    say "Starting checker-enabled Zone; log: $ZONE_LOG"
    (
        cd "$ZONES_ROOT" || exit
        export RUST_LOG="${RUST_LOG:-info,zone::checker=debug}"
        export RUST_LOG_STYLE=never
        exec "$ZONE_BIN" node \
            --chain "$ZONE_DIR/genesis.json" --datadir "$ZONE_DATADIR" \
            --l1.rpc-url "$L1_WS_URL" --l1.portal-address "$portal" --zone.id "$zone_id" \
            --http --http.addr 127.0.0.1 --http.port "$ZONE_HTTP_PORT" --http.api all \
            --metrics "127.0.0.1:$ZONE_METRICS_PORT" \
            --redacted-rpc.port "$ZONE_REDACTED_PORT" \
            --log.file.directory "$ZONE_DIR/logs" \
            --sequencer --sequencer-key-file "$key" \
            --zone.batch-interval-blocks "$ZONE_BATCH_INTERVAL_BLOCKS" \
            --checker.mode observe
    ) >"$ZONE_LOG" 2>&1 &
    echo "$!" >"$ZONE_PID_FILE"
    wait_for_rpc "$ZONE_HTTP_URL" "$ZONE_PID_FILE" "Zone" "$ZONE_LOG"
    wait_for_metric reth_tempo_zone_checker_verified_zone_height
}

up() { prepare; start_l1; prepare_l1_protocol; build_zone; provision_zone; start_zone; status; }
restart_zone() { prepare; assert_running "$L1_PID_FILE" "Tempo" "$L1_LOG"; stop_zone; build_zone; start_zone; status; }
down() { prepare; stop_zone; stop_one "$L1_PID_FILE" "Tempo"; }
reset() { down; say "Removing disposable lab state: $STATE_DIR"; rm -rf -- "$STATE_DIR"; }
