#!/usr/bin/env bash
# Start a Nitro Enclave running the zone sequencer.
#
# Architecture:
#   ┌──────────────────────────────┐
#   │        EC2 Instance          │
#   │                              │
#   │  ┌────────────────────────┐  │
#   │  │    Nitro Enclave       │  │
#   │  │                        │  │
#   │  │  tempo-zone            │  │
#   │  │  (zone sequencer)      │  │
#   │  │                        │  │
#   │  │  vsock CID:port ◄──────┼──┼── vsock-proxy ── L1 RPC
#   │  └────────────────────────┘  │
#   └──────────────────────────────┘
#
# The enclave is fully isolated — no network, no disk, no external access.
# Communication with the parent instance happens exclusively over vsock.
# A vsock proxy on the parent forwards L1 RPC traffic into the enclave.
#
# Prerequisites:
#   - EC2 instance with Nitro Enclave support enabled
#   - nitro-cli installed
#   - EIF built via build-enclave.sh
#
# Usage:
#   ./enclave/run-enclave.sh <eif-path> [--cpu-count 2] [--memory 4096]
set -euo pipefail

ENCLAVE_NAME="tempo-zone"
CPU_COUNT=2
MEMORY_MB=4096
EIF_PATH=""
VSOCK_PROXY_PORT=8000
L1_RPC_HOST="rpc.moderato.tempo.xyz"
L1_RPC_PORT=443

# Parse arguments
if [[ $# -lt 1 ]]; then
    echo "Usage: $0 <eif-path> [--cpu-count N] [--memory MB] [--vsock-port PORT] [--l1-rpc-host HOST] [--l1-rpc-port PORT]" >&2
    exit 1
fi

EIF_PATH="$1"
shift

while [[ $# -gt 0 ]]; do
    case "$1" in
        --cpu-count)  CPU_COUNT="$2";       shift 2 ;;
        --memory)     MEMORY_MB="$2";       shift 2 ;;
        --vsock-port) VSOCK_PROXY_PORT="$2"; shift 2 ;;
        --l1-rpc-host) L1_RPC_HOST="$2";    shift 2 ;;
        --l1-rpc-port) L1_RPC_PORT="$2";    shift 2 ;;
        *)
            echo "Unknown option: $1" >&2
            exit 1
            ;;
    esac
done

if [[ ! -f "$EIF_PATH" ]]; then
    echo "Error: EIF file not found: $EIF_PATH" >&2
    exit 1
fi

# Check prerequisites
for cmd in nitro-cli; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "Error: '$cmd' is required but not found in PATH." >&2
        exit 1
    fi
done

echo "============================================"
echo "  Starting Nitro Enclave"
echo "============================================"
echo ""
echo "  EIF:        $EIF_PATH"
echo "  CPUs:       $CPU_COUNT"
echo "  Memory:     ${MEMORY_MB}MB"
echo ""

# Step 1: Terminate any existing enclave
echo "Step 1: Checking for existing enclaves..."
EXISTING=$(nitro-cli describe-enclaves 2>/dev/null | jq -r '.[].EnclaveID // empty')
if [[ -n "$EXISTING" ]]; then
    echo "  Terminating existing enclave: $EXISTING"
    nitro-cli terminate-enclave --enclave-id "$EXISTING"
    echo "  Terminated."
fi
echo ""

# Step 2: Start the enclave
echo "Step 2: Starting enclave..."
RUN_OUTPUT=$(nitro-cli run-enclave \
    --eif-path "$EIF_PATH" \
    --cpu-count "$CPU_COUNT" \
    --memory "$MEMORY_MB" \
    --enclave-name "$ENCLAVE_NAME")
echo "$RUN_OUTPUT"

ENCLAVE_ID=$(echo "$RUN_OUTPUT" | jq -r '.EnclaveID')
ENCLAVE_CID=$(echo "$RUN_OUTPUT" | jq -r '.EnclaveCID')

if [[ -z "$ENCLAVE_ID" || "$ENCLAVE_ID" == "null" ]]; then
    echo "Error: Failed to start enclave." >&2
    exit 1
fi
echo ""

# Step 3: Start vsock proxy for L1 RPC access
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROXY_SCRIPT="$SCRIPT_DIR/vsock-proxy.py"

echo "Step 3: Starting vsock proxy..."
echo "  vsock port:  $VSOCK_PROXY_PORT"
echo "  target:      $L1_RPC_HOST:$L1_RPC_PORT"

if [[ -f "$PROXY_SCRIPT" ]]; then
    python3 "$PROXY_SCRIPT" \
        --vsock-port "$VSOCK_PROXY_PORT" \
        --target-host "$L1_RPC_HOST" \
        --target-port "$L1_RPC_PORT" &
    PROXY_PID=$!
    echo "  Proxy PID:   $PROXY_PID"
else
    echo "  Warning: vsock-proxy.py not found at $PROXY_SCRIPT"
    echo "  Start the proxy manually."
fi
echo ""

echo "============================================"
echo "  Enclave Running"
echo "============================================"
echo ""
echo "  Enclave ID:  $ENCLAVE_ID"
echo "  Enclave CID: $ENCLAVE_CID"
echo ""
echo "  To view console output:"
echo "    nitro-cli console --enclave-id $ENCLAVE_ID"
echo ""
echo "  To terminate:"
echo "    nitro-cli terminate-enclave --enclave-id $ENCLAVE_ID"
