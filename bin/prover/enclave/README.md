# Tempo Zone SPF enclave service

`tempo-zone-prover-enclave` runs the Zone stateless proof function inside an AWS Nitro Enclave. The
parent instance generates a complete `BatchWitness` and sends it to the enclave over `AF_VSOCK`;
the enclave performs no RPC or filesystem access.

## Protocol

The server listens on AF_VSOCK port `5000` by default. Every connection uses TLS 1.3 with a
single-use enclave key bound to fresh Nitro evidence; there is no plaintext mode. The host proxy
forwards the challenge preface and TLS ciphertext without seeing witness or response plaintext.
Inside TLS, JSON messages use the chunked framing protocol (1 MiB per frame, 512 MiB per request by
default).

Requests use this envelope:

```json
{
  "version": 1,
  "requestId": "caller-selected-id",
  "witness": {}
}
```

`witness` is the serde representation of `zone_spf::BatchWitness`. The prover accepts chain IDs
compiled into Tempo plus custom genesis files configured by the enclave operator through a
`--tempo-genesis` directory. A request cannot supply its own chain specification. Responses have
`status: "ok"` with a `zone_spf::BatchOutput`, or `status: "error"` with a stable `code` and a
diagnostic `message`.

After successful SPF execution, the enclave derives the canonical Zone batch digest and asks the
Nitro Secure Module to place it in the signed attestation document's `user_data`. A successful
response includes `proofBundle.verifierConfig = 0x01` and the raw COSE/CBOR document in
`proofBundle.proof`. The prover returns `attestation_unavailable` when `/dev/nsm` is unavailable or
the NSM request fails.

Pass `--use-tcp` to listen on localhost TCP instead of AF_VSOCK. This works on every supported
operating system; AF_VSOCK remains the default and is available only on Linux. Set `SPF_PORT` or
pass `--port` to change the selected transport's port. The maximum request payload can be changed
with `SPF_MAX_REQUEST_BYTES` or `--max-request-bytes`.

TCP mode is for enclave-side development and still requires the Nitro Secure Module.
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

Each TLS connection generates a fresh private key inside the enclave. The EIF builder therefore
forces `random.trust_bootloader=off random.trust_cpu=off`, and enclave startup fails closed unless
`rng_current` is `nsm-hwrng`. Certificate freshness comes from the client nonce and NSM-signed
timestamp, so the enclave does not need a trusted wall clock.
Operational timeouts affect only liveness and can use a monotonic clock.

The host image launches the enclave in non-debug mode and exposes TCP port `5000`. It accepts
`PROVER_EIF_PATH`, `ENCLAVE_NAME`, `ENCLAVE_CPU_COUNT`, `ENCLAVE_MEMORY_MIB`, `ENCLAVE_CID`,
`PROVER_TCP_PORT`, `PROVER_VSOCK_PORT`, and `MONITOR_INTERVAL_SECONDS` as runtime configuration.
