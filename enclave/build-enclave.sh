#!/usr/bin/env bash
# Build a Nitro Enclave Image File (EIF) from the zone sequencer and output PCR measurements.
#
# The EIF bundles the Docker image into a single file that can be loaded into a
# Nitro Enclave. The build also produces PCR (Platform Configuration Register)
# measurements that uniquely identify the enclave contents:
#
#   PCR0 — Hash of the enclave image (code + data)
#   PCR1 — Hash of the Linux kernel used inside the enclave
#   PCR2 — Hash of the application (user-space binary + filesystem)
#
# These PCRs can be registered on-chain so anyone can verify that a specific
# enclave is running exactly the expected code.
#
# Prerequisites:
#   - Docker
#   - AWS Nitro CLI (nitro-cli): https://docs.aws.amazon.com/enclaves/latest/user/nitro-enclave-cli.html
#   - cast (from Foundry) — for keccak256 hashing
#
# Usage:
#   ./enclave/build-enclave.sh [--output-dir <dir>]
#
# Outputs:
#   <output-dir>/tempo-zone.eif       — Enclave Image File
#   <output-dir>/measurements.json    — PCR0, PCR1, PCR2 hashes + metadata
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="enclave/out"
DOCKER_IMAGE="tempo-zone-enclave"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --output-dir)
            OUTPUT_DIR="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1" >&2
            echo "Usage: $0 [--output-dir <dir>]" >&2
            exit 1
            ;;
    esac
done

# Check prerequisites
for cmd in docker nitro-cli cast jq git; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "Error: '$cmd' is required but not found in PATH." >&2
        exit 1
    fi
done

mkdir -p "$OUTPUT_DIR"

GIT_COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD)"
GIT_COMMIT_SHORT="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
VERSION="$(grep '^version' "$REPO_ROOT/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')"

echo "============================================"
echo "  Building Nitro Enclave Image"
echo "============================================"
echo ""
echo "  Version:    $VERSION"
echo "  Commit:     $GIT_COMMIT_SHORT ($GIT_COMMIT)"
echo "  Output:     $OUTPUT_DIR/"
echo ""

# Step 1: Build the deterministic Docker image
echo "Step 1: Building Docker image..."
docker build \
    -f "$REPO_ROOT/enclave/Dockerfile.enclave" \
    -t "$DOCKER_IMAGE" \
    "$REPO_ROOT"
echo ""

# Step 2: Build the EIF using nitro-cli
echo "Step 2: Building Enclave Image File (EIF)..."
BUILD_OUTPUT=$(nitro-cli build-enclave \
    --docker-uri "$DOCKER_IMAGE" \
    --output-file "$OUTPUT_DIR/tempo-zone.eif")
echo "$BUILD_OUTPUT"
echo ""

# Step 3: Extract PCR measurements from nitro-cli output
PCR0=$(echo "$BUILD_OUTPUT" | jq -r '.Measurements.PCR0')
PCR1=$(echo "$BUILD_OUTPUT" | jq -r '.Measurements.PCR1')
PCR2=$(echo "$BUILD_OUTPUT" | jq -r '.Measurements.PCR2')

if [[ -z "$PCR0" || "$PCR0" == "null" ]]; then
    echo "Error: Failed to extract PCR measurements from nitro-cli output." >&2
    exit 1
fi

# Step 4: Compute measurementHash = keccak256(PCR0 || PCR1 || PCR2)
# Strip "0x" prefixes if present, concatenate, then hash.
PCR0_HEX="${PCR0#0x}"
PCR1_HEX="${PCR1#0x}"
PCR2_HEX="${PCR2#0x}"
MEASUREMENT_HASH=$(cast keccak "0x${PCR0_HEX}${PCR1_HEX}${PCR2_HEX}")

# Step 5: Write measurements.json
BUILD_TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
cat > "$OUTPUT_DIR/measurements.json" <<EOF
{
  "version": "$VERSION",
  "gitCommit": "$GIT_COMMIT",
  "buildTimestamp": "$BUILD_TIMESTAMP",
  "pcr0": "$PCR0",
  "pcr1": "$PCR1",
  "pcr2": "$PCR2",
  "measurementHash": "$MEASUREMENT_HASH"
}
EOF

echo "============================================"
echo "  Enclave Build Complete"
echo "============================================"
echo ""
echo "  EIF:              $OUTPUT_DIR/tempo-zone.eif"
echo "  Measurements:     $OUTPUT_DIR/measurements.json"
echo ""
echo "  PCR0 (image):     $PCR0"
echo "  PCR1 (kernel):    $PCR1"
echo "  PCR2 (app):       $PCR2"
echo ""
echo "  measurementHash:  $MEASUREMENT_HASH"
echo ""
echo "To verify a reproducible build, rebuild from the same commit and"
echo "compare the PCR values and measurementHash."
