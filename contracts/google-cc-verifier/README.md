# Google Confidential Space Validator

This Foundry subproject contains a Google-only PKI attestation validator for Confidential Space workloads running on Intel TDX.

The design separates two concerns:

- `GooglePkiAttestationVerifier.sol` verifies the pinned root, intermediate, and leaf RSA certificates, verifies the RS256 JWS signature, base64url-decodes the payload, and extracts only the claims the policy actually checks.
- `GoogleConfidentialSpaceValidator.sol` enforces a small policy surface: expected `eat_nonce`, expected image digest, `GCP_INTEL_TDX`, production `dbgstat`, and token freshness.

The verifier is intentionally not a generic claim validator. It only extracts the fields the policy contract needs.

## Security Model

This project assumes you are comfortable trusting Google as the attestation verifier.

What the current implementation gives you:

- Google signed the attestation token through the verified PKI chain.
- The token claims the workload ran in `GCP_INTEL_TDX`.
- The token claims the workload was the exact pinned Confidential Space image digest.
- The token claims `dbgstat == disabled-since-boot`.
- `eat_nonce` binds the attestation to a caller-chosen challenge.
- The token is recent according to `iat`, `nbf`, and `exp`.

What it does not give you:

- A proof that the audited image is semantically correct.
- A proof that the software cannot exfiltrate data beyond what your audited code and runtime restrictions guarantee.

## Flow

1. The caller chooses an `expectedEatNonce` challenge.
2. The workload requests a Google PKI attestation token with that nonce in `eat_nonce`.
3. The verifier checks the certificate chain and JWS signature, then extracts only the required claims.
4. The validator checks issuer, `GCP_INTEL_TDX`, production `dbgstat`, expected image digest, token freshness, and `eat_nonce`.

If you want to bind additional request data such as a block hash, fold that into the nonce before requesting the token.

## Layout

- `src/interfaces/IGooglePkiAttestationVerifier.sol`
  Proof shape and normalized claims returned by the verifier.
- `src/GooglePkiAttestationVerifier.sol`
  Concrete Google PKI verifier with RSA, ASN.1, X.509, and narrow payload parsing.
- `src/GoogleConfidentialSpaceValidator.sol`
  Policy contract that validates issuer, TDX, debug state, image digest, freshness, and `eat_nonce`.
- `src/libraries/GoogleAttestationPayloadParser.sol`
  Narrow claim extractor for the required Google Confidential Space payload fields.
- `src/libraries/Base64Url.sol`
  Strict base64url wrapper around Solady's decoder for the JWS payload segment.
- `src/libraries/Asn1Decode.sol`
  Minimal ASN.1 / DER navigation helpers for certificate parsing.
- `PARSING.md`
  Walkthrough of how the certificate chain and payload are parsed end to end.
- `test/GoogleConfidentialSpaceValidatorE2E.t.sol`
  End-to-end test covering real cert-chain verification, JWS verification, and policy enforcement.
- `script/generate_google_pki_fixture.py`
  Fixture generator for the local RSA root/intermediate/leaf chain and signed test token.

## Measured Gas

From the current test suite:

- `GooglePkiAttestationVerifier.verifyAndExtract`: about `0.75M` gas on the success path.
- `GoogleConfidentialSpaceValidator.verifyAttestation`: about `0.76M` gas on the full success path.
- Tampered-JWS rejection path: about `0.32M` gas.

## Testing

Run from this directory:

```sh
forge build
forge test -vv
forge test --gas-report
```

To regenerate the local cryptographic fixture:

```sh
python3 script/generate_google_pki_fixture.py
```
