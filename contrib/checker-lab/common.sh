# shellcheck shell=bash
# Shared checker-lab process, build, and metric helpers.

say() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
require_command() { command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"; }

prepare() {
    for command in cargo cast curl git jq just; do require_command "$command"; done
    [[ -d "$TEMPO_ROOT" ]] || die "Tempo checkout not found: $TEMPO_ROOT (set TEMPO_ROOT)"
    mkdir -p "$GENESIS_DIR" "$L1_DATADIR" "$ZONE_DIR" "$LOG_DIR" "$PID_DIR"
}

validate_tempo_checkout() {
    [[ -n "$TEMPO_BIN_OVERRIDE" ]] && return
    local expected actual
    expected="$(sed -nE 's/.*tempo-alloy.*rev = "([0-9a-f]+)".*/\1/p' "$ZONES_ROOT/Cargo.toml" | head -n 1)"
    [[ -n "$expected" ]] || die "could not determine pinned Tempo revision"
    actual="$(git -C "$TEMPO_ROOT" rev-parse HEAD)"
    [[ "$actual" == "$expected" ]] || die "Tempo is at $actual; Zones requires $expected"
}

pid_is_running() {
    [[ -f "$1" ]] || return 1
    local pid; pid="$(<"$1")"
    [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null
}

assert_running() {
    pid_is_running "$1" && return
    printf '%s is not running. Last log lines:\n' "$2" >&2
    tail -n 40 "$3" >&2 2>/dev/null || true
    exit 1
}

wait_for_rpc() {
    local url="$1" pid_file="$2" name="$3" log="$4"
    for ((i = 0; i < 120; i++)); do
        assert_running "$pid_file" "$name" "$log"
        cast block-number --rpc-url "$url" >/dev/null 2>&1 && return
        sleep 1
    done
    die "timed out waiting for $name RPC at $url"
}

metric() {
    curl -fsS "$ZONE_METRICS_URL" | awk -v name="$1" '$1 == name { value = $2 } END { print value }'
}

wait_for_metric() {
    local name="$1"
    for ((i = 0; i < 120; i++)); do
        [[ -n "$(metric "$name" 2>/dev/null)" ]] && return
        sleep 1
    done
    die "timed out waiting for checker metric $name"
}

build_tempo() {
    validate_tempo_checkout
    if [[ -z "$TEMPO_BIN_OVERRIDE" ]]; then
        say "Building Tempo"
        cargo build --manifest-path "$TEMPO_ROOT/Cargo.toml" --bin tempo
    fi
    [[ -x "$TEMPO_BIN" ]] || die "Tempo binary not found: $TEMPO_BIN"
}

build_zone() {
    [[ -n "$ZONE_BIN_OVERRIDE" ]] || cargo build --manifest-path "$ZONES_ROOT/Cargo.toml" --bin tempo-zone
    [[ -x "$ZONE_BIN" ]] || die "Zone binary not found: $ZONE_BIN"
}

stop_one() {
    local pid_file="$1" name="$2"
    if ! pid_is_running "$pid_file"; then rm -f "$pid_file"; return; fi
    local pid; pid="$(<"$pid_file")"
    printf 'Stopping %s (PID %s)\n' "$name" "$pid"
    kill -INT "$pid" 2>/dev/null || true
    for ((i = 0; i < 40; i++)); do kill -0 "$pid" 2>/dev/null || break; sleep 0.25; done
    kill -0 "$pid" 2>/dev/null && kill -TERM "$pid" 2>/dev/null || true
    rm -f "$pid_file"
}
