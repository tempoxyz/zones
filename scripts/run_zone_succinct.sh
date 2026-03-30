#!/usr/bin/env bash
set -euo pipefail

# Launch the zone node with the Succinct/SP1 prover backend.
# This validates the minimum env required for a real network proof path.
#
# Required env:
#   L1_RPC_URL
#   L1_PORTAL_ADDRESS
#   SEQUENCER_KEY
#   NETWORK_PRIVATE_KEY   (Succinct prover network funded key)
#
# Optional env:
#   NETWORK_RPC_URL
#   ZONE_PROVER_SUCCINCT_SKIP_SIMULATION=true|false
#   ZONE_PROVER_BACKEND (forced to "succinct" if unset)
#
# Any CLI args passed to this script are forwarded to `tempo-zone`.

require_env() {
  local name="$1"
  if [[ -z "${!name:-}" ]]; then
    echo "missing required env: ${name}" >&2
    exit 1
  fi
}

require_env "L1_RPC_URL"
require_env "L1_PORTAL_ADDRESS"
require_env "SEQUENCER_KEY"
require_env "NETWORK_PRIVATE_KEY"

export ZONE_PROVER_BACKEND="${ZONE_PROVER_BACKEND:-succinct}"

if [[ "${ZONE_PROVER_BACKEND}" != "succinct" ]]; then
  echo "ZONE_PROVER_BACKEND must be 'succinct' (got: ${ZONE_PROVER_BACKEND})" >&2
  exit 1
fi

echo "Starting tempo-zone with Succinct prover backend..."
echo "  backend: ${ZONE_PROVER_BACKEND}"
echo "  skip simulation: ${ZONE_PROVER_SUCCINCT_SKIP_SIMULATION:-false}"
if [[ -n "${NETWORK_RPC_URL:-}" ]]; then
  echo "  network rpc url: ${NETWORK_RPC_URL}"
fi

exec cargo run -p tempo-zone --features succinct-prover -- "$@"
