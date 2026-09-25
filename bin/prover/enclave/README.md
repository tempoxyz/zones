# Tempo Zone SPF enclave service

`tempo-zone-prover-enclave` runs the Zone stateless proof function inside an AWS Nitro Enclave. The
client generates a complete `BatchWitness` and sends it over Nitro-attested TLS through the host's
TCP-to-`AF_VSOCK` proxy. SPF execution performs no RPC or filesystem access.

## Protocol

The server listens on AF_VSOCK port `5000` by default, or on TCP port `5000` when `--use-tcp` is
enabled. Each connection authenticates the enclave, carries one request and one response inside
TLS 1.3, then closes. The CBOR framing inside TLS is unchanged: a four-byte, big-endian payload
length followed by the payload.

At startup the enclave generates one in-memory self-signed certificate and private key. The
plaintext bootstrap consists of `TZRATLS2` followed by a 32-byte client-generated random nonce.
The enclave replies with two length-prefixed frames: certificate DER (at most 4096 bytes) and a
fresh NSM attestation (at most 24576 bytes). Its `nonce` echoes the challenge; `user_data` is
`SHA256(b"tempo-zone-prover/tls-bootstrap/v1\0" || certificate_DER)`. The certificate is always
the enclave's own, never supplied by the client.

The client verifies the AWS certificate chain and COSE signature, PCR policy, timestamp, nonce,
and certificate binding before using that certificate as its only rustls trust anchor. Normal
TLS verification checks the `tempo-zone-prover.invalid` name and proves possession of the private
key. Session resumption and early data are disabled. No witness is sent before authentication.
The host proxy forwards bootstrap bytes and TLS ciphertext without interpreting either.
The enclave serves one connection at a time, which bounds multi-GiB witness memory; a stalled
handshake delays other clients by at most the handshake timeout. The bootstrap adds one round trip
before each TLS handshake.
Both endpoints bound the complete handshake to ten seconds (including TCP connect on the client).
The server authenticates itself; this does not add client authorization or prevent host denial of
service. The batch attestation used for on-chain settlement remains separate and unchanged.

Requests use the serde representation of `zone_prover::VerifyRequest` with protocol version `2`.
The witness's byte-heavy fields are encoded as CBOR byte strings rather than human-readable hex.
Decoding is schema-driven and rejects unknown, duplicate, or trailing request data. The prover
accepts chain IDs compiled into Tempo plus custom genesis files configured by the enclave operator
through a `--tempo-genesis` directory. A request cannot supply its own chain
specification. Responses use the externally tagged `zone_prover::VerifyResponse`: `ok` includes a
`zone_spf::BatchOutput`, while `error` includes a stable `code` and diagnostic `message`.

After successful SPF execution, the enclave derives the canonical Zone batch digest and asks the
Nitro Secure Module to place it in the signed attestation document's `user_data`. A successful
response includes `proofBundle.verifierConfig = 0x01` and the raw COSE/CBOR document in
`proofBundle.proof`. The prover returns `attestation_unavailable` when `/dev/nsm` is unavailable or
the NSM request fails.

Pass `--use-tcp` to listen on localhost TCP instead of AF_VSOCK. Both modes require Nitro;
AF_VSOCK remains the default. Set `SPF_PORT` or
pass `--port` to change the selected transport's port. The maximum request payload defaults to 2
GiB and can be changed with `SPF_MAX_REQUEST_BYTES` or `--max-request-bytes`. The host runner
allocates 10 GiB to the enclave by default; override it with `ENCLAVE_MEMORY_MIB`.

The enclave applies separate absolute deadlines to request reception and response transmission.
`--request-timeout-secs` (`SPF_REQUEST_TIMEOUT_SECS`, default 300) covers reception and decoding of
the complete CBOR request. `--response-timeout-secs` (`SPF_RESPONSE_TIMEOUT_SECS`, default 300)
covers encoding and transmission of every normal or error response. The generous five-minute
defaults accommodate multi-GiB payloads while still recovering from crashed clients. Values are
whole seconds and must be greater than zero; progress does not reset a deadline. SPF execution
itself has no timeout.

TCP mode uses the same attestation and entropy requirements as AF_VSOCK. Local protocol tests use
in-memory streams and a test-only signed attestation chain; the binary has no plaintext fallback.
Set `SPF_TEMPO_GENESIS` or pass `--tempo-genesis` with a directory containing trusted Tempo genesis
JSON files. Files are loaded in filename order. Each custom chain ID must be unique and cannot
override a built-in Tempo network.

## Images and EIF

The published `ghcr.io/tempoxyz/tempo-zone-prover` image is the Nitro host image to run on an
enclave-enabled node. It contains the enclave EIF, Nitro CLI, and the TCP-to-vsock proxy. The
enclave payload is an intermediate image and is not published separately.

To build the same artifacts locally, first load the payload into the local Docker image store:

```console
docker buildx bake \
  -f docker/docker-bake.hcl \
  --load \
  --set tempo-zone-prover-enclave.tags=tempo-zone-prover-enclave:local \
  tempo-zone-prover-enclave
```

CI supplies the downloaded devnet genesis as a named build context. The enclave payload stores it
in `/etc/tempo/genesis/` and passes that directory to the prover explicitly through the image
entrypoint.

Convert the payload to an EIF with the repository's pinned Nitro CLI builder image, then build the
host image:

```console
docker buildx bake \
  -f docker/docker-bake.hcl \
  --load \
  --set tempo-zone-prover-eif-builder.tags=tempo-zone-prover-eif-builder:local \
  tempo-zone-prover-eif-builder
mkdir -p target/tempo-zone-prover-eif
docker run --rm \
  --platform linux/amd64 \
  --volume /var/run/docker.sock:/var/run/docker.sock \
  --volume "$PWD/target/tempo-zone-prover-eif:/output" \
  tempo-zone-prover-eif-builder:local \
  build-enclave \
  --docker-uri tempo-zone-prover-enclave:local \
  --output-file /output/tempo-zone-prover.eif \
  | tee target/tempo-zone-prover-eif/measurements.json
docker buildx bake \
  -f docker/docker-bake.hcl \
  --load \
  --set tempo-zone-prover.tags=tempo-zone-prover:local \
  tempo-zone-prover
```

The EIF and PCR measurements are written under `target/tempo-zone-prover-eif/`. CI embeds the EIF
in the published image and uploads the measurements as a commit-specific workflow artifact.

The EIF uses Linux 6.6.79 and its matching NSM driver, built from a pinned AWS Nitro bootstrap
commit. Changing either one changes the EIF PCR measurements, so the expected measurements must
also be updated.

TLS keys require trusted randomness. The EIF builder sets
`random.trust_bootloader=off random.trust_cpu=off`; startup checks these flags and requires
`rng_current` to be `nsm-hwrng` before generating the key, including in TCP mode. The enclave does
not require a wall clock: certificate validity spans 2024–9999 and the client checks NSM-signed
timestamps and fresh nonces. Clients require an accurate wall clock. The default maximum evidence
age is 300 seconds, with at most 300 seconds of future clock skew.

Remote clients must supply a PCR0–2 allowlist with `--sequencer.prover-attestation-policy` (node)
or `--attestation-policy` (prover utils); see the [policy example](../utils/README.md). Debug-mode
zero PCRs are rejected. Rebuild the EIF and distribute its trusted measurements when deploying
this change; coordinate client/server upgrades because plaintext clients are no longer accepted.

The host image launches the enclave in non-debug mode and exposes TCP port `5000`. It accepts
`PROVER_EIF_PATH`, `ENCLAVE_NAME`, `ENCLAVE_CPU_COUNT`, `ENCLAVE_MEMORY_MIB`, `ENCLAVE_CID`,
`PROVER_TCP_PORT`, `PROVER_VSOCK_PORT`, and `MONITOR_INTERVAL_SECONDS` as runtime configuration.
