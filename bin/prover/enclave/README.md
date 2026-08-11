# Tempo Zone SPF enclave service

`tempo-zone-prover-enclave` runs the Zone stateless proof function inside an AWS Nitro Enclave. The
parent instance generates a complete `BatchWitness` and sends it to the enclave over `AF_VSOCK`;
the enclave performs no RPC or filesystem access.

## Protocol

The server listens on vsock port `5000` by default. Each connection carries one request and one
response, then closes. A frame consists of a four-byte, big-endian payload length followed by a
UTF-8 JSON payload.

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
compiled into Tempo plus custom genesis files configured by the enclave operator through repeated
`--tempo-genesis` arguments. A request cannot supply its own chain specification. Responses have
`status: "ok"` with a `zone_spf::BatchOutput`, or `status: "error"` with a stable `code` and a
diagnostic `message`.

Set `SPF_VSOCK_PORT` or pass `--port` to change the port. The maximum request payload defaults to
512 MiB and can be changed with `SPF_MAX_REQUEST_BYTES` or `--max-request-bytes`.
Set `SPF_TEMPO_GENESIS` to a comma-separated list or pass `--tempo-genesis` repeatedly for trusted
Tempo genesis JSON files. Each custom chain ID must be unique and cannot override a built-in Tempo
network.

## Image and EIF

Build the OCI image with:

```console
docker buildx bake tempo-zone-prover
```

Release images are published as `ghcr.io/tempoxyz/tempo-zone-prover`. Resolve an immutable image
digest and convert it to an Enclave Image File on Linux with the Nitro CLI:

```console
nitro-cli build-enclave \
  --docker-uri ghcr.io/tempoxyz/tempo-zone-prover@sha256:<digest> \
  --output-file tempo-zone-prover.eif
```

Retain the PCR measurements printed by `build-enclave` for future attestation policy. Attestation
exchange and the parent-side vsock client are intentionally outside this service.
