# Parsing Walkthrough

This note explains the two custom parsing paths in the validator:

- DER / X.509 parsing for the Google certificate chain.
- JSON claim extraction for the Google attestation payload.

The goal is not to describe ASN.1 or JWT generically. It is to show exactly what this code expects and why.

## Certificate Parsing

The verifier parses three RSA certificates:

1. a pinned root certificate set in the constructor
2. an intermediate certificate from the proof
3. a leaf certificate from the proof

The chain logic lives in `src/GooglePkiAttestationVerifier.sol`.

### `Asn1Ptr`

`src/libraries/Asn1Decode.sol` uses a tiny packed pointer type called `Asn1Ptr`.

Each pointer stores three offsets for one DER node:

- `headerOffset`: where the tag byte starts
- `contentOffset`: where the value bytes start
- `contentLength`: how many value bytes belong to the node

From those three values we can also compute `encodedLength`, which is the full TLV length from tag byte through value bytes.

This lets the parser walk the DER tree without allocating nested objects.

### Navigation Model

The parser only needs four navigation operations:

- `root(der)`: read the first DER node in a byte string
- `firstChildOf(der, ptr)`: move into a constructed value like a `SEQUENCE`
- `nextSiblingOf(der, ptr)`: move to the next TLV immediately after the current node
- `rootOf(der, ptr)`: treat the current node's content bytes as a new standalone DER blob

`rootOf` is used when an object is wrapped inside a `BIT STRING` or `OCTET STRING`. For example, RSA public keys inside `SubjectPublicKeyInfo` are wrapped that way.

### Top-Level Certificate Shape

The verifier expects the normal X.509 layout:

```text
Certificate ::= SEQUENCE {
  tbsCertificate       TBSCertificate,
  signatureAlgorithm   AlgorithmIdentifier,
  signatureValue       BIT STRING
}
```

`_parseCertificate` does exactly that:

1. `root()` reads the outer `SEQUENCE`
2. `firstChildOf()` moves to `tbsCertificate`
3. `nextSiblingOf()` moves to `signatureAlgorithm`
4. the outer algorithm is checked to be `sha256WithRSAEncryption`
5. `_parseTbsCertificate()` extracts the fields the chain validator needs

Separately, `_certificateSignature()` reads the outer `signatureValue` and unwraps the `BIT STRING`.

### TBS Certificate Shape

The verifier supports the narrow v3 shape it expects from Google's chain:

```text
TBSCertificate ::= SEQUENCE {
  version            [0] EXPLICIT Version,
  serialNumber            CertificateSerialNumber,
  signature               AlgorithmIdentifier,
  issuer                  Name,
  validity                Validity,
  subject                 Name,
  subjectPublicKeyInfo    SubjectPublicKeyInfo,
  issuerUniqueID     [1]  IMPLICIT OPTIONAL,
  subjectUniqueID    [2]  IMPLICIT OPTIONAL,
  extensions         [3]  EXPLICIT Extensions
}
```

`_parseTbsCertificate` walks those fields in order.

The important outputs are:

- `issuerHash`: `keccak256` of the raw `Name` value bytes
- `subjectHash`: `keccak256` of the raw `Name` value bytes
- `notAfter`: parsed from the `Validity` sequence
- `ca`: extracted from `basicConstraints`
- `modulus` and `exponent`: extracted from `SubjectPublicKeyInfo`

The code does not try to canonicalize `Name` objects into strings. It hashes the exact DER value bytes and compares parent subject vs. child issuer on that normalized byte representation.

### Why Unique IDs Are Skipped

X.509 allows optional `issuerUniqueID` and `subjectUniqueID` fields before extensions.

This verifier does not use them, but it skips over tags `0x81` and `0x82` if they are present so it can still reach the required `[3]` extensions wrapper.

### Extensions the Verifier Actually Uses

The chain validator only enforces two extensions:

- `basicConstraints`
- `keyUsage`

That logic is in `_verifyExtensions`.

For `basicConstraints`, the verifier checks whether the certificate should be a CA or an end-entity cert.

For `keyUsage`, it checks:

- CA certs must have `keyCertSign`
- leaf certs must have `digitalSignature`

Other extensions are ignored because they are not needed for this narrow validation path.

### RSA Public Key Parsing

`SubjectPublicKeyInfo` has another small wrapper shape:

```text
SubjectPublicKeyInfo ::= SEQUENCE {
  algorithm         AlgorithmIdentifier,
  subjectPublicKey  BIT STRING
}

RSAPublicKey ::= SEQUENCE {
  modulus           INTEGER,
  publicExponent    INTEGER
}
```

`_parseRsaPublicKey` verifies that the key algorithm is plain RSA encryption, unwraps the bit string, then reads the modulus and exponent integers.

### Signature Verification Flow

For a child certificate, `_verifyCertificate` does four checks:

1. parent cert must itself be a CA
2. parent must still be within its validity window
3. child `issuerHash` must equal parent `subjectHash`
4. parent's RSA key must verify the child's `sha256(tbsCertificate)` signature

The root certificate is treated similarly in the constructor, except it is expected to be self-signed.

## JWS Payload Parsing

After the chain is verified, the leaf RSA key is used to verify the attestation signature over `signingInput`.

`signingInput` is expected to be the detached compact-JWS prefix:

```text
base64url(header) + "." + base64url(payload)
```

The signature bytes are passed separately in the proof.

`_decodePayload` does three things:

1. find the single `.` separator
2. slice out the payload segment after the separator
3. strict-base64url decode it with `Base64Url.decode`

The strict wrapper matters because Solady's raw base64 decoder is permissive on malformed input.

## Claim Extraction

`src/libraries/GoogleAttestationPayloadParser.sol` is intentionally not a generic JSON parser.

It extracts only the claims used by `GoogleConfidentialSpaceValidator`:

- `iss`
- `eat_nonce`
- `iat`
- `nbf`
- `exp`
- `hwmodel`
- `dbgstat`
- `submods.container.image_digest`

Every required field must appear exactly once. If a required key is duplicated, parsing reverts with `duplicate claim`.

That is deliberate: the policy should never depend on whichever duplicate value a JSON library happens to return.

### `eat_nonce`

Google can encode `eat_nonce` in two accepted shapes:

- a single hex string
- a singleton array containing one hex string

The parser accepts both, but requires the decoded value to fit exactly into `bytes32`.

## Policy Enforcement

Once claims are extracted, `src/GoogleConfidentialSpaceValidator.sol` applies the actual policy:

- issuer must be Google's expected issuer URL
- `eat_nonce` must match the caller-supplied challenge
- `hwmodel` must be `GCP_INTEL_TDX`
- `dbgstat` must be `disabled-since-boot`
- `image_digest` must match the pinned digest
- `nbf`, `iat`, and `exp` must be consistent with the current block timestamp
- optional max token age must hold

That is the full trust boundary of the current contract.

## Deliberate Non-Goals

The current code does not attempt to be:

- a full X.509 validator
- a full JWT / OIDC implementation
- a generic JSON query engine
- an independent Intel TDX quote verifier

That narrow scope is intentional. The custom code is limited to the exact parsing needed for the onchain Google PKI verification path.
