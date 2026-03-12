#!/usr/bin/env bash
set -euo pipefail

# Launch the zone node with the Nitro TEE proof backend.
#
# This backend sends BatchWitness payloads to an external TEE service and uses
# the returned enclave signature as the proof payload (no SP1 dependency).
#
# Required env:
#   L1_RPC_URL
#   L1_PORTAL_ADDRESS
#   SEQUENCER_KEY
#   ZONE_PROVER_TEE_ENDPOINT
#
# Optional env:
#   ZONE_PROVER_TEE_TIMEOUT_MS
#   ZONE_PROVER_TEE_MAX_RESPONSE_BYTES
#   ZONE_PROVER_TEE_EXPECTED_SIGNER
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
require_env "ZONE_PROVER_TEE_ENDPOINT"

export ZONE_PROVER_BACKEND="nitro-tee"

echo "Starting tempo-zone with Nitro TEE backend..."
echo "  backend: ${ZONE_PROVER_BACKEND}"
echo "  endpoint: ${ZONE_PROVER_TEE_ENDPOINT}"
echo "  timeout ms: ${ZONE_PROVER_TEE_TIMEOUT_MS:-30000}"
echo "  max response bytes: ${ZONE_PROVER_TEE_MAX_RESPONSE_BYTES:-1048576}"
if [[ -n "${ZONE_PROVER_TEE_EXPECTED_SIGNER:-}" ]]; then
  echo "  expected signer: ${ZONE_PROVER_TEE_EXPECTED_SIGNER}"
fi

exec cargo run -p tempo-zone -- "$@"
