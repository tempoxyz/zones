# Tempo Zone SPF enclave service

`tempo-zone-prover-enclave` runs the Zone stateless proof function inside an AWS Nitro Enclave. The
parent instance generates a complete `BatchWitness` and sends it to the enclave over `AF_VSOCK`;
the enclave performs no RPC or filesystem access.

## Protocol

The server listens on AF_VSOCK port `5000` by default, or on TCP port `5000` when `--use-tcp` is
enabled. Each connection carries one request and one response, then closes. A frame consists of a
four-byte, big-endian payload length followed by a UTF-8 JSON payload.

Requests use this envelope:

```json
{
  "version": 1,
  "requestId": "caller-selected-id",
  "tempoChainId": 42431,
  "witness": {}
}
```

`witness` is the serde representation of `zone_spf::BatchWitness`. The prover accepts chain IDs
compiled into Tempo plus custom genesis files configured by the enclave operator through a
`--tempo-genesis` directory. A request cannot supply its own chain specification. Responses have
`status: "ok"` with a `zone_spf::BatchOutput`, or `status: "error"` with a stable `code` and a
diagnostic `message`.

Pass `--use-tcp` to listen on localhost TCP instead of AF_VSOCK. This works on every supported
operating system; AF_VSOCK remains the default and is available only on Linux. Set `SPF_PORT` or
pass `--port` to change the selected transport's port. The maximum request payload defaults to 512
MiB and can be changed with `SPF_MAX_REQUEST_BYTES` or `--max-request-bytes`.
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
  --load \
  --set tempo-zone-prover-enclave.tags=tempo-zone-prover-enclave:local \
  tempo-zone-prover-enclave
```

CI accepts one Tempo genesis URL per line through the `tempo_genesis_urls` workflow input and
supplies the downloaded files as a named build context. The enclave payload stores them in
`/etc/tempo/genesis/` and passes that directory to the prover explicitly through the image
entrypoint.

Convert the payload to an EIF with the repository's pinned Nitro CLI builder image, then build the
host image:

```console
docker buildx bake \
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
  --load \
  --set tempo-zone-prover.tags=tempo-zone-prover:local \
  tempo-zone-prover
```

The EIF and PCR measurements are written under `target/tempo-zone-prover-eif/`. CI embeds the EIF
in the published image and uploads the measurements as a commit-specific workflow artifact.

The host image launches the enclave in non-debug mode and exposes TCP port `5000`. It accepts
`PROVER_EIF_PATH`, `ENCLAVE_NAME`, `ENCLAVE_CPU_COUNT`, `ENCLAVE_MEMORY_MIB`, `ENCLAVE_CID`,
`PROVER_TCP_PORT`, `PROVER_VSOCK_PORT`, and `MONITOR_INTERVAL_SECONDS` as runtime configuration.
