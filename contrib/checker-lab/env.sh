# Shared checker-lab configuration.

ZONES_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
readonly ZONES_ROOT
readonly TEMPO_ROOT="${TEMPO_ROOT:-$(cd -- "$ZONES_ROOT/.." && pwd)/tempo}"
readonly STATE_DIR="${CHECKER_LAB_STATE_DIR:-$ZONES_ROOT/target/checker-lab}"

readonly TEMPO_BIN_OVERRIDE="${TEMPO_BIN:-}"
readonly ZONE_BIN_OVERRIDE="${ZONE_BIN:-}"
readonly TEMPO_BIN="${TEMPO_BIN:-$TEMPO_ROOT/target/debug/tempo}"
readonly ZONE_BIN="${ZONE_BIN:-$ZONES_ROOT/target/debug/tempo-zone}"
readonly L1_HTTP_PORT="${L1_HTTP_PORT:-8545}"
readonly L1_WS_PORT="${L1_WS_PORT:-8546}"
readonly L1_AUTH_PORT="${L1_AUTH_PORT:-8551}"
readonly L1_P2P_PORT="${L1_P2P_PORT:-30303}"
readonly ZONE_HTTP_PORT="${ZONE_HTTP_PORT:-9545}"
readonly ZONE_REDACTED_PORT="${ZONE_REDACTED_PORT:-9555}"
readonly ZONE_METRICS_PORT="${ZONE_METRICS_PORT:-9001}"
readonly L1_BLOCK_TIME="${L1_BLOCK_TIME:-500ms}"

readonly L1_HTTP_URL="http://127.0.0.1:$L1_HTTP_PORT"
readonly L1_WS_URL="ws://127.0.0.1:$L1_WS_PORT"
readonly ZONE_HTTP_URL="http://127.0.0.1:$ZONE_HTTP_PORT"
readonly ZONE_METRICS_URL="http://127.0.0.1:$ZONE_METRICS_PORT/metrics"

# Standard first Anvil development account. Never use this lab configuration on a public network.
readonly DEV_KEY="${DEV_KEY:-0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
readonly DEV_ADDRESS="${DEV_ADDRESS:-0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266}"
readonly PATH_USD="0x20C0000000000000000000000000000000000000"
readonly ALPHA_USD="0x20c0000000000000000000000000000000000001"
readonly TIP403_REGISTRY="0x403C000000000000000000000000000000000000"
readonly ZONE_FACTORY="0x5AF2000000000000000000000000000000000000"

readonly GENESIS_DIR="$STATE_DIR/genesis"
readonly L1_DATADIR="$STATE_DIR/l1"
readonly ZONE_DIR="$STATE_DIR/zone"
readonly ZONE_DATADIR="$ZONE_DIR/node"
readonly CHECKER_DB="$STATE_DIR/checker"
readonly LOG_DIR="$STATE_DIR/logs"
readonly PID_DIR="$STATE_DIR/pids"
readonly L1_LOG="$LOG_DIR/l1.log"
readonly ZONE_LOG="$LOG_DIR/zone.log"
readonly L1_PID_FILE="$PID_DIR/l1.pid"
readonly ZONE_PID_FILE="$PID_DIR/zone.pid"
