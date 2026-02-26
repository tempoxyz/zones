# Nitro Attestation Verification

## Architecture

The attestation verification follows a two-step approach optimized for
time-to-market:

### Step 1: Off-chain Attestation Verification (this component)

The attestation verifier runs on the parent EC2 instance and:
1. Requests a Nitro attestation document from the enclave via vsock
2. Verifies the COSE_Sign1 signature chain to AWS Nitro root CA
3. Validates PCR values match the expected measurements from reproducible builds
4. Extracts the enclave's signing public key from the attestation's `user_data`
5. Signs a registration statement with the trusted `attestationSigner` key

### Step 2: On-chain Registration (NitroVerifier.sol)

The signed registration statement is submitted to the NitroVerifier contract,
which stores the enclave key and measurement hash for the portal.

### Why not verify on-chain?

Nitro attestation documents use:
- COSE_Sign1 envelope (CBOR-encoded)
- ES384 (P-384 ECDSA) signatures
- X.509 certificate chains

None of these have cheap EVM precompiles. Implementing CBOR parsing + P-384
verification in Solidity would be prohibitively expensive (>1M gas) and
error-prone.

The two-step approach costs ~50,000 gas per batch (2x ecrecover + hashing).

## Trust Model

### On-chain trust anchor
- `attestationSigner` address stored in NitroVerifier
- Should be a multisig/governance key
- Only signs registration statements after verifying Nitro attestation

### What the enclave proves
- Code integrity: PCR0 matches deterministic build of zone sequencer
- Runtime integrity: PCR1 matches expected kernel
- Application integrity: PCR2 matches application config
- Key binding: enclave signing key is generated inside the enclave and
  included in the attestation's `user_data`

### Security guarantees
1. **Code authenticity**: Only the exact zone sequencer binary (reproducible build)
   can produce valid batch signatures
2. **Key isolation**: The enclave signing key never leaves the enclave
3. **Replay protection**: Batch digests include chain ID, verifier address, portal
   address, and withdrawal batch index
4. **Expiration**: Registrations expire after a configurable duration, requiring
   periodic re-attestation

## PCR Values

| PCR | Contents | Description |
|-----|----------|-------------|
| PCR0 | Enclave image measurement | Hash of the EIF (Enclave Image File) |
| PCR1 | Kernel measurement | Hash of the enclave's Linux kernel |
| PCR2 | Application measurement | Hash of the application + init ramdisk |

The `measurementHash` stored on-chain is `keccak256(PCR0 || PCR1 || PCR2)`.

## Key Rotation

1. Build new enclave image → new PCR values
2. Deploy new enclave, generate new signing key inside it
3. Get Nitro attestation with new key in `user_data`
4. Verify attestation, sign new registration with `attestationSigner`
5. Submit registration tx → NitroVerifier updates the portal's enclave key
6. Old key immediately becomes invalid (new registration overwrites)

## Future: Full On-chain Verification

When available (e.g., via a precompile or ZK proof):
- Add a P-384 verification precompile to Tempo
- Verify Nitro COSE/CBOR directly in NitroVerifier
- Remove the trusted `attestationSigner` from the trust model
