# shellcheck shell=bash
# Tempo lifecycle for the checker lab.

generate_genesis() {
    [[ -f "$GENESIS_DIR/genesis.json" ]] && return
    say "Generating Tempo development genesis"
    cargo run --manifest-path "$TEMPO_ROOT/Cargo.toml" -p tempo-xtask -- generate-genesis \
        --output "$GENESIS_DIR" --accounts 1000 --no-dkg-in-genesis

    local factory slot current owner desired temporary
    factory="$(printf '%s' "$ZONE_FACTORY" | tr '[:upper:]' '[:lower:]')"
    slot="0x$(printf '%064d' 0)"
    current="$(jq -er --arg factory "$factory" --arg slot "$slot" \
        '.alloc[$factory].storage[$slot]' "$GENESIS_DIR/genesis.json")" \
        || die "Tempo genesis is missing the ZoneFactory configuration slot"
    [[ "$current" =~ ^0x[0-9a-fA-F]{64}$ && "${current: -8}" == "00000001" ]] \
        || die "unexpected ZoneFactory configuration"
    owner="$(printf '%s' "${DEV_ADDRESS#0x}" | tr '[:upper:]' '[:lower:]')"
    desired="0x0000000000000000${owner}00000001"
    temporary="$GENESIS_DIR/genesis.json.tmp"
    jq --arg factory "$factory" --arg slot "$slot" --arg value "$desired" \
        '.alloc[$factory].storage[$slot] = $value' "$GENESIS_DIR/genesis.json" >"$temporary"
    mv "$temporary" "$GENESIS_DIR/genesis.json"
}

start_l1() {
    pid_is_running "$L1_PID_FILE" && return
    cast block-number --rpc-url "$L1_HTTP_URL" >/dev/null 2>&1 \
        && die "$L1_HTTP_URL is occupied by a process not owned by the lab"
    build_tempo
    generate_genesis
    say "Starting Tempo; log: $L1_LOG"
    (
        cd "$TEMPO_ROOT" || exit
        exec "$TEMPO_BIN" node \
            --chain "$GENESIS_DIR/genesis.json" --datadir "$L1_DATADIR" \
            --dev --dev.block-time "$L1_BLOCK_TIME" \
            --http --http.addr 127.0.0.1 --http.port "$L1_HTTP_PORT" --http.api all \
            --ws --ws.addr 127.0.0.1 --ws.port "$L1_WS_PORT" --ws.api all \
            --authrpc.port "$L1_AUTH_PORT" --port "$L1_P2P_PORT" \
            --engine.disable-precompile-cache --engine.legacy-state-root \
            --builder.gaslimit 3000000000 --builder.max-tasks 1 --builder.deadline 3 \
            --faucet.enabled --faucet.private-key "$DEV_KEY" \
            --faucet.amount 1000000000000000 --faucet.address "$PATH_USD" "$ALPHA_USD"
    ) >"$L1_LOG" 2>&1 &
    echo "$!" >"$L1_PID_FILE"
    wait_for_rpc "$L1_HTTP_URL" "$L1_PID_FILE" "Tempo" "$L1_LOG"
}

prepare_l1_protocol() {
    local owner balance configured
    owner="$(cast call "$ZONE_FACTORY" 'owner()(address)' --rpc-url "$L1_HTTP_URL" | tr '[:upper:]' '[:lower:]')"
    [[ "$owner" == "$(printf '%s' "$DEV_ADDRESS" | tr '[:upper:]' '[:lower:]')" ]] \
        || die "local ZoneFactory is not owned by the development account"
    balance="$(cast call "$PATH_USD" 'balanceOf(address)(uint256)' "$DEV_ADDRESS" --rpc-url "$L1_HTTP_URL" | awk '{print $1}')"
    [[ "$balance" != 0 ]] || cast rpc tempo_fundAddress "$DEV_ADDRESS" --rpc-url "$L1_HTTP_URL" >/dev/null
    for token in "$PATH_USD" "$ALPHA_USD"; do
        configured="$(cast call "$TIP403_REGISTRY" 'tokenTransferPolicyId(address)(bool,uint64)' "$token" --rpc-url "$L1_HTTP_URL" | head -n 1)"
        [[ "$configured" == true ]] || cast send "$TIP403_REGISTRY" \
            'migrateTransferPolicyIds(address[])' "[$token]" --private-key "$DEV_KEY" \
            --rpc-url "$L1_HTTP_URL" >/dev/null
    done
}
