#!/usr/bin/env bash
# Register an enclave signing key on the NitroVerifier contract.
#
# This is the "bridge" step between off-chain Nitro attestation verification
# and on-chain enclave key registration. In production, this script would:
#   1. Connect to the enclave via vsock
#   2. Request a Nitro attestation document
#   3. Verify the attestation (COSE/CBOR + AWS root certs + PCR values)
#   4. Extract the enclave's public key from the attestation
#   5. Sign a registration statement with the attestation signer key
#   6. Submit the registration tx to L1
#
# For MVP/development, it takes the enclave key directly and signs the
# registration with the attestation signer key.
#
# Usage:
#   ./enclave/register-enclave-key.sh \
#     --enclave-key <hex> \
#     --measurement-hash <hex> \
#     --attester-key <hex> \
#     --verifier-address <addr> \
#     --portal-address <addr> \
#     --sequencer-address <addr> \
#     --expires-in <seconds> \
#     --rpc-url <url>
set -euo pipefail

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
ENCLAVE_KEY=""
MEASUREMENT_HASH=""
ATTESTER_KEY=""
VERIFIER_ADDRESS=""
PORTAL_ADDRESS=""
SEQUENCER_ADDRESS=""
EXPIRES_IN=""
RPC_URL=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --enclave-key)      ENCLAVE_KEY="$2";      shift 2 ;;
        --measurement-hash) MEASUREMENT_HASH="$2"; shift 2 ;;
        --attester-key)     ATTESTER_KEY="$2";     shift 2 ;;
        --verifier-address) VERIFIER_ADDRESS="$2"; shift 2 ;;
        --portal-address)   PORTAL_ADDRESS="$2";   shift 2 ;;
        --sequencer-address) SEQUENCER_ADDRESS="$2"; shift 2 ;;
        --expires-in)       EXPIRES_IN="$2";       shift 2 ;;
        --rpc-url)          RPC_URL="$2";          shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

# ---------------------------------------------------------------------------
# Validate required arguments
# ---------------------------------------------------------------------------
: "${ENCLAVE_KEY:?--enclave-key is required (private key hex of the enclave signing key)}"
: "${MEASUREMENT_HASH:?--measurement-hash is required (bytes32 keccak of PCR0||PCR1||PCR2)}"
: "${ATTESTER_KEY:?--attester-key is required (private key hex of the attestation signer)}"
: "${VERIFIER_ADDRESS:?--verifier-address is required (NitroVerifier contract address)}"
: "${PORTAL_ADDRESS:?--portal-address is required (ZonePortal contract address)}"
: "${SEQUENCER_ADDRESS:?--sequencer-address is required (sequencer address for the zone)}"
: "${EXPIRES_IN:?--expires-in is required (registration validity duration in seconds)}"
: "${RPC_URL:?--rpc-url is required (L1 RPC endpoint)}"

# Ensure RPC URL uses HTTP (cast send requires it)
HTTP_RPC=$(echo "$RPC_URL" | sed 's|^wss://|https://|' | sed 's|^ws://|http://|')

# ---------------------------------------------------------------------------
# Derive addresses and compute expiration
# ---------------------------------------------------------------------------

# Derive the enclave key's Ethereum address from its private key
ENCLAVE_KEY_ADDRESS=$(cast wallet address "$ENCLAVE_KEY")
echo "Enclave key address: $ENCLAVE_KEY_ADDRESS"

# Derive the attester's address (for logging)
ATTESTER_ADDRESS=$(cast wallet address "$ATTESTER_KEY")
echo "Attester address:    $ATTESTER_ADDRESS"

# Compute the expiration timestamp (current time + expires_in seconds)
NOW=$(date +%s)
EXPIRES_AT=$((NOW + EXPIRES_IN))
echo "Expires at:          $EXPIRES_AT ($(date -d @"$EXPIRES_AT" 2>/dev/null || date -r "$EXPIRES_AT" 2>/dev/null || echo "in ${EXPIRES_IN}s"))"

# Get the chain ID from the RPC
CHAIN_ID=$(cast chain-id --rpc-url "$HTTP_RPC")
echo "Chain ID:            $CHAIN_ID"

# Read the current registration nonce for this portal from the contract
NONCE=$(cast call "$VERIFIER_ADDRESS" "registrationNonce(address)(uint64)" "$PORTAL_ADDRESS" --rpc-url "$HTTP_RPC" 2>/dev/null || echo "0")
echo "Registration nonce:  $NONCE"
echo ""

# ---------------------------------------------------------------------------
# Step 1: Compute the registration digest
#
# This must match NitroVerifier.sol's digest computation:
#   keccak256(abi.encode(
#     "NitroVerifier.RegisterEnclaveKey",
#     chainId,
#     verifierAddress,
#     portalAddress,
#     sequencerAddress,
#     enclaveKeyAddress,
#     measurementHash,
#     expiresAt,
#     nonce
#   ))
#
# We use `cast abi-encode` to produce the same encoding as Solidity's
# abi.encode(), then hash it with keccak256.
# ---------------------------------------------------------------------------
echo "Computing registration digest..."

# abi.encode the registration message — string is encoded as a dynamic type
ABI_ENCODED=$(cast abi-encode \
    "f(string,uint256,address,address,address,address,bytes32,uint256,uint256)" \
    "NitroVerifier.RegisterEnclaveKey" \
    "$CHAIN_ID" \
    "$VERIFIER_ADDRESS" \
    "$PORTAL_ADDRESS" \
    "$SEQUENCER_ADDRESS" \
    "$ENCLAVE_KEY_ADDRESS" \
    "$MEASUREMENT_HASH" \
    "$EXPIRES_AT" \
    "$NONCE")

# Hash the ABI-encoded data to get the digest
DIGEST=$(cast keccak "$ABI_ENCODED")
echo "Digest:              $DIGEST"

# ---------------------------------------------------------------------------
# Step 2: Sign the digest with the attester key
#
# We use `cast wallet sign --no-hash` because we've already computed the
# keccak256 digest — we want the attester to sign the raw 32-byte hash
# without any additional hashing or EIP-191 prefix.
# ---------------------------------------------------------------------------
echo "Signing digest with attester key..."

SIGNATURE=$(cast wallet sign --no-hash "$DIGEST" --private-key "$ATTESTER_KEY")
echo "Signature:           ${SIGNATURE:0:20}..."
echo ""

# ---------------------------------------------------------------------------
# Step 3: Submit the registration transaction
#
# Call registerEnclaveKey() on the NitroVerifier contract.
# The function signature:
#   registerEnclaveKey(
#     address portalAddress,
#     address sequencerAddress,
#     address enclaveKeyAddress,
#     bytes32 measurementHash,
#     uint64 expiresAt,
#     uint64 nonce,
#     bytes signature
#   )
#
# The attester key is used as the transaction sender's private key for gas,
# but the signature parameter is what the contract actually verifies.
# ---------------------------------------------------------------------------
echo "Submitting registration transaction..."

TX_OUTPUT=$(cast send "$VERIFIER_ADDRESS" \
    "registerEnclaveKey(address,address,address,bytes32,uint64,uint64,bytes)" \
    "$PORTAL_ADDRESS" \
    "$SEQUENCER_ADDRESS" \
    "$ENCLAVE_KEY_ADDRESS" \
    "$MEASUREMENT_HASH" \
    "$EXPIRES_AT" \
    "$NONCE" \
    "$SIGNATURE" \
    --rpc-url "$HTTP_RPC" \
    --private-key "$ATTESTER_KEY" \
    --json)

TX_HASH=$(echo "$TX_OUTPUT" | jq -r '.transactionHash')
TX_STATUS=$(echo "$TX_OUTPUT" | jq -r '.status')

echo ""
echo "============================================"
echo "  Enclave Key Registration"
echo "============================================"
echo ""
echo "  Status:          $TX_STATUS"
echo "  Tx Hash:         $TX_HASH"
echo "  Enclave Key:     $ENCLAVE_KEY_ADDRESS"
echo "  Portal:          $PORTAL_ADDRESS"
echo "  Sequencer:       $SEQUENCER_ADDRESS"
echo "  Measurement:     $MEASUREMENT_HASH"
echo "  Expires At:      $EXPIRES_AT"
echo ""

if [[ "$TX_STATUS" != "0x1" ]]; then
    echo "WARNING: Transaction may have reverted. Check the explorer." >&2
    exit 1
fi

echo "Enclave key registered successfully!"
