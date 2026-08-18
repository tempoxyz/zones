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
  up                              Start Tempo and a checker-enabled Zone
  trigger token|deposit|withdrawal|all
                                  Submit activity and await verification
  restart-zone                    Rebuild and restart the Zone
  status                          Show processes and checker metrics
  logs [zone|l1]                  Follow a managed log
  down                            Stop processes and preserve state
  reset                           Stop processes and remove lab state
  help                            Show this help

Set TEMPO_ROOT when the pinned Tempo checkout is not ../tempo.
EOF
}

case "${1:-}" in
    up) up ;;
    trigger) trigger "${2:-}" ;;
    restart-zone) restart_zone ;;
    status) status ;;
    logs) logs "${2:-zone}" ;;
    down) down ;;
    reset) reset ;;
    help | --help | -h) usage ;;
    *) usage; exit 1 ;;
esac
