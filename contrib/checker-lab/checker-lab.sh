#!/usr/bin/env bash

set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR

# shellcheck source=env.sh
source "$SCRIPT_DIR/env.sh"
# shellcheck source=common.sh
source "$SCRIPT_DIR/common.sh"
# shellcheck source=l1.sh
source "$SCRIPT_DIR/l1.sh"
# shellcheck source=zone.sh
source "$SCRIPT_DIR/zone.sh"
# shellcheck source=scenarios.sh
source "$SCRIPT_DIR/scenarios.sh"
# shellcheck source=status.sh
source "$SCRIPT_DIR/status.sh"

usage() {
    cat <<EOF
Usage: $(basename "$0") <command> [argument]

Commands:
  up                         Build and start Tempo L1 and the Zone checker
  restart-zone               Rebuild and restart only the Zone checker
  trigger token|deposit|withdrawal
                             Submit bridge activity and await checker progress
  status                     Show processes and durable checker state
  logs [zone|l1]             Follow a managed log
  down                       Stop managed processes and preserve state
  reset                      Stop processes and remove lab state
  help                       Show this help

Environment overrides:
  TEMPO_ROOT, TEMPO_BIN, ZONE_BIN, CHECKER_LAB_STATE_DIR,
  L1_HTTP_PORT, L1_WS_PORT, L1_AUTH_PORT, L1_P2P_PORT,
  ZONE_HTTP_PORT, ZONE_REDACTED_PORT, ZONE_METRICS_PORT, L1_BLOCK_TIME
EOF
}

case "${1:-}" in
    up) up ;;
    restart-zone) restart_zone ;;
    trigger) trigger "${2:-}" ;;
    status) status ;;
    logs) logs "${2:-zone}" ;;
    down) down ;;
    reset) reset ;;
    help | --help | -h) usage ;;
    *) usage; exit 1 ;;
esac
