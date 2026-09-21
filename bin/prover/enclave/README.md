# Tempo Zone SPF enclave service

`tempo-zone-prover-enclave` runs the Zone stateless proof function inside an AWS Nitro Enclave. The
parent instance generates a complete `BatchWitness` and sends it to the enclave over `AF_VSOCK`;
the enclave performs no RPC or filesystem access.

## Protocol

The server listens on AF_VSOCK port `5000` by default, or on TCP port `5000` when `--use-tcp` is
enabled. Each connection carries one request and one response, then closes. A frame consists of a
four-byte, big-endian payload length followed by a CBOR payload.

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

Pass `--use-tcp` to listen on localhost TCP instead of AF_VSOCK. This works on every supported
operating system; AF_VSOCK remains the default and is available only on Linux. Set `SPF_PORT` or
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

TCP mode is intended for development of framing, chain validation, and SPF error handling. The
binary still requires the Nitro Secure Module after a successful SPF replay, so a valid request run
outside an enclave ends with `attestation_unavailable` rather than an unattested success response.
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

Verifying a batch does not use local randomness or wall-clock time. If we add key or nonce
generation or KMS/HTTPS calls, configure `random.trust_bootloader=off random.trust_cpu=off` and
require `rng_current` to be `nsm-hwrng`. If we add KMS/HTTPS calls, expiring credentials, protocol
timestamps, or time-based replay checks, use `kvm-clock`. Operational timeouts affect only liveness
and can use a monotonic clock.

The host image launches the enclave in non-debug mode and exposes TCP port `5000`. It accepts
`PROVER_EIF_PATH`, `ENCLAVE_NAME`, `ENCLAVE_CPU_COUNT`, `ENCLAVE_MEMORY_MIB`, `ENCLAVE_CID`,
`PROVER_TCP_PORT`, `PROVER_VSOCK_PORT`, and `MONITOR_INTERVAL_SECONDS` as runtime configuration.

### Upgrading across an L1 hardfork

Keep separate deployments of the currently approved EIF and the next hardfork's EIF. Pin each
host image by digest, retain its commit-specific PCR measurements, and verify those measurements
against the PCR policy shipped in the corresponding L1 release. Do not repoint an existing
endpoint to a different EIF during the transition. The node does not authenticate an endpoint's
advertised release; L1 remains responsible for checking the attestation's PCRs.

Configure the node with one exact `--sequencer.prover-address` assignment per L1 hardfork.
For example, a T12/T13 transition uses:

```sh
--sequencer.enable-prover \
--sequencer.prover-address T12=prover-t12:5000 \
--sequencer.prover-address T13=prover-t13:5000
```

The equivalent environment setting is
`SEQUENCER_PROVER_ADDRESS=T12=prover-t12:5000,T13=prover-t13:5000`.
Use the actual forks supported by the node binary; a future T14 assignment requires a binary
whose Tempo dependency recognizes T14. Assign the same endpoint explicitly to adjacent forks
when the accepted prover image is unchanged. At startup, sequencers with proving enabled and
remote shadow provers require an assignment for the current L1 hardfork and every later Tempo
fork in the node's chainspec activating within 24 hours of startup (inclusive). Overdue forks
not yet active on a lagging L1 also require an assignment. Forks more than 24 hours away do not.
This checks the live fork and configured addresses, not endpoint connectivity or PCRs. Missing
assignments and unknown L1 forks stop proving; there is no fallback to an older endpoint.

Before activation, bring up the next deployment and exercise it on historical and mixed-fork
witnesses. The new prover must preserve historical execution rules so it can attest unsettled
pre-upgrade batches after old PCRs are retired. The node hardfork settlement integration test
exercises SPF recovery across the T12/T13 transition; repeat that validation for each new STF release.

The sequencer selects an endpoint using the live L1 hardfork when it sends a proving request,
not the hardfork of the batch's imported L1 block. It rechecks the selected fork before
settlement, during quorum waits, and before broadcast. A fork change rebuilds the attempt with
a fresh proof; retries reconcile portal progress before deciding to submit again. Remote shadow
proving also uses the live fork's endpoint to validate historical submissions.

When colocating releases, use distinct `ENCLAVE_NAME`, `ENCLAVE_CID`, and `PROVER_TCP_PORT`
settings and reserve enough Nitro CPU and memory for both. Separate hosts can use the defaults.
Retain the old deployment through the operational reorg window, then retire it after the new
fork is stable. Retaining a deployment does not extend acceptance of its PCRs on L1.

Watch `tempo_zone_monitor_prover_hardfork_rebuild_total`, settlement lag, and the
`Selected remote prover` log (endpoint, hardfork, Zone range) during the transition.
