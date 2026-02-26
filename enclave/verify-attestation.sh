#!/usr/bin/env bash
# Verify a Nitro Enclave attestation document.
#
# In production, this script:
#   1. Requests an attestation doc from the enclave via vsock
#   2. Parses the COSE_Sign1 envelope
#   3. Verifies the certificate chain to AWS Nitro root CA
#   4. Extracts PCR values (PCR0=enclave image, PCR1=kernel, PCR2=application)
#   5. Compares PCR values against expected measurements
#   6. Extracts the enclave's public key from user_data
#   7. Outputs the verified enclave key and measurement hash
#
# Prerequisites:
#   - AWS Nitro root CA certificate
#   - Expected PCR values from measurements.json
#   - Access to the enclave via vsock
#
# For MVP, this script validates inputs and demonstrates the flow.
#
# Usage:
#   ./enclave/verify-attestation.sh \
#     --vsock-cid <cid> \
#     --vsock-port <port> \
#     --expected-pcr0 <hex> \
#     --expected-pcr1 <hex> \
#     --expected-pcr2 <hex>
#
# Output (on success):
#   ENCLAVE_KEY=0x<public-key-hex>
#   MEASUREMENT_HASH=0x<keccak256-of-pcr0||pcr1||pcr2>
set -euo pipefail

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
VSOCK_CID=""
VSOCK_PORT="5000"
EXPECTED_PCR0=""
EXPECTED_PCR1=""
EXPECTED_PCR2=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --vsock-cid)     VSOCK_CID="$2";     shift 2 ;;
        --vsock-port)    VSOCK_PORT="$2";     shift 2 ;;
        --expected-pcr0) EXPECTED_PCR0="$2";  shift 2 ;;
        --expected-pcr1) EXPECTED_PCR1="$2";  shift 2 ;;
        --expected-pcr2) EXPECTED_PCR2="$2";  shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

: "${EXPECTED_PCR0:?--expected-pcr0 is required (expected enclave image measurement)}"
: "${EXPECTED_PCR1:?--expected-pcr1 is required (expected kernel measurement)}"
: "${EXPECTED_PCR2:?--expected-pcr2 is required (expected application measurement)}"

echo "============================================"
echo "  Nitro Attestation Verification"
echo "============================================"
echo ""

# ---------------------------------------------------------------------------
# Step 1: Request attestation document from the enclave
#
# In production, this uses the vsock (AF_VSOCK) protocol to communicate
# with the enclave. The enclave generates a fresh attestation document
# containing:
#   - PCR values (measurements of the enclave image/kernel/app)
#   - user_data: the enclave's signing public key
#   - nonce: optional freshness value
#   - A COSE_Sign1 signature over the above
#
# The attestation document is a CBOR-encoded COSE_Sign1 structure
# signed by the Nitro hypervisor's attestation key (ES384 / P-384).
# ---------------------------------------------------------------------------
echo "Step 1: Requesting attestation document from enclave..."

if [[ -n "$VSOCK_CID" ]]; then
    echo "  Would connect to vsock CID=$VSOCK_CID port=$VSOCK_PORT"
    echo "  TODO: Implement vsock client to request attestation document"
    echo "  The enclave exposes an attestation endpoint that returns a"
    echo "  CBOR-encoded COSE_Sign1 document when queried."
else
    echo "  [MVP] No vsock CID provided — skipping attestation request."
    echo "  In production, this step fetches the raw attestation bytes."
fi
echo ""

# ---------------------------------------------------------------------------
# Step 2: Parse the COSE_Sign1 envelope
#
# A COSE_Sign1 structure contains:
#   [protected_headers, unprotected_headers, payload, signature]
#
# The payload is a CBOR map with:
#   - module_id: string identifying the enclave
#   - timestamp: attestation generation time (ms since epoch)
#   - digest: "SHA384"
#   - pcrs: map of PCR index → PCR value (48 bytes each for SHA-384)
#   - certificate: DER-encoded attestation certificate
#   - cabundle: array of DER-encoded CA certificates
#   - public_key: optional (enclave-provided)
#   - user_data: optional (enclave-provided — we put the signing pubkey here)
#   - nonce: optional (caller-provided freshness value)
# ---------------------------------------------------------------------------
echo "Step 2: Parsing COSE_Sign1 envelope..."
echo "  TODO: Parse CBOR-encoded COSE_Sign1 structure"
echo "  Libraries: python3 -c 'import cbor2, cose' or openssl + custom parser"
echo ""

# ---------------------------------------------------------------------------
# Step 3: Verify certificate chain to AWS Nitro root CA
#
# The attestation document contains a certificate chain:
#   Nitro Root CA → Intermediate CA → Attestation Certificate
#
# We verify:
#   a) The root CA matches AWS's published Nitro Attestation Root CA
#      (fingerprint: 8cf60e2b2efca96c6a9e71e851d00e0418ccead826798275cf7393bc5b907752)
#   b) Each certificate in the chain is valid and properly signed
#   c) The attestation certificate signed the COSE_Sign1 payload
#
# The signature algorithm is ES384 (ECDSA with P-384 + SHA-384).
# This is NOT compatible with EVM's ecrecover (which uses secp256k1).
# ---------------------------------------------------------------------------
echo "Step 3: Verifying certificate chain to AWS Nitro root CA..."
echo "  TODO: Download and pin AWS Nitro Root CA certificate"
echo "  TODO: Verify certificate chain using openssl verify"
echo "  TODO: Verify COSE_Sign1 signature using ES384"
echo ""

# ---------------------------------------------------------------------------
# Step 4: Extract and validate PCR values
#
# PCR (Platform Configuration Register) values are SHA-384 hashes:
#   PCR0 — Hash of the enclave image (EIF file)
#   PCR1 — Hash of the Linux kernel and boot ramdisk
#   PCR2 — Hash of the application (user-space code + init ramdisk)
#   PCR3 — Hash of the IAM role (if assigned)
#   PCR4 — Hash of the instance ID
#   PCR8 — Hash of the signing certificate (if EIF was signed)
#
# We only check PCR0-PCR2 for code integrity verification.
# ---------------------------------------------------------------------------
echo "Step 4: Validating PCR values..."
echo "  Expected PCR0: $EXPECTED_PCR0"
echo "  Expected PCR1: $EXPECTED_PCR1"
echo "  Expected PCR2: $EXPECTED_PCR2"
echo ""
echo "  TODO: Extract actual PCR values from attestation payload"
echo "  TODO: Compare PCR0 against expected (enclave image hash)"
echo "  TODO: Compare PCR1 against expected (kernel hash)"
echo "  TODO: Compare PCR2 against expected (application hash)"
echo ""

# ---------------------------------------------------------------------------
# Step 5: Extract the enclave's public key from user_data
#
# When the enclave generates its signing keypair (secp256k1), it places
# the public key in the attestation document's `user_data` field.
# This binds the signing key to the attestation — proving the key was
# generated inside the specific enclave instance.
# ---------------------------------------------------------------------------
echo "Step 5: Extracting enclave public key from user_data..."
echo "  TODO: Extract user_data field from attestation payload"
echo "  TODO: Parse as 20-byte Ethereum address or 33/65-byte public key"
echo ""

# ---------------------------------------------------------------------------
# Step 6: Compute measurement hash for on-chain registration
#
# The on-chain NitroVerifier stores a single bytes32 measurement hash
# rather than individual PCR values. This is computed as:
#   measurementHash = keccak256(PCR0 || PCR1 || PCR2)
#
# This allows the on-chain contract to verify that a registration
# was for a specific enclave build without storing all three PCR values.
# ---------------------------------------------------------------------------
echo "Step 6: Computing measurement hash..."
echo "  measurementHash = keccak256(PCR0 || PCR1 || PCR2)"
echo "  TODO: Concatenate PCR values and hash with keccak256"
echo ""

# ---------------------------------------------------------------------------
# Output (in production, this would print verified values)
# ---------------------------------------------------------------------------
echo "============================================"
echo "  Verification Result: STUB (MVP)"
echo "============================================"
echo ""
echo "  In production, this script would output:"
echo "    ENCLAVE_KEY=0x<verified-enclave-address>"
echo "    MEASUREMENT_HASH=0x<keccak256-of-pcrs>"
echo ""
echo "  These values would then be passed to register-enclave-key.sh"
echo "  to complete the on-chain registration."
echo ""
echo "  See enclave/attestation/README.md for the full architecture."
