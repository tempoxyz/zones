# Reproducible build verification

## Candidate binary

The Docker Build workflow can compare the `tempo-zone` binary in a Depot-built
candidate image with an independent clean rebuild of the same source commit.
The orchestration lives in `tempoxyz/gh-actions`; Zones owns the Dockerfile, Bake
target, Cargo profile, and `scripts/reproducible-build.sh`.

After the shared workflow and Zones caller are merged, dispatch from `main`:

```sh
gh workflow run docker.yml --repo tempoxyz/zones --ref main \
  -f reproducible_verify=true -f ref=<source-commit-sha>
gh run list --repo tempoxyz/zones --workflow docker.yml \
  --event workflow_dispatch --limit 5
gh run watch <run-id> --repo tempoxyz/zones --exit-status
```

Alternatively, open **Actions → Docker Build → Run workflow**, select `main`,
enable `reproducible_verify`, and enter the source ref. Omitting the ref builds
that workflow run's commit. The verification option skips normal image publishing
and publishes only a run-specific candidate under
`ghcr.io/tempoxyz/tempo-zone-repro`.

Successful runs complete all four shared jobs: resolve, candidate, rebuild, and
compare. A skipped verification job is not verification. The published image's
binary must match the clean rebuild; a mismatch fails the run.

Download and inspect the comparison manifest:

```sh
gh run download <run-id> --repo tempoxyz/zones \
  -n reproducible-candidate-binary-verification -D verification
jq -e '.binary_comparison_result == "success" and
       .depot_sha256 == .clean_build_sha256' \
  verification/reproducible-image-verification.json
```

The manifest records the source commit, trusted recipe commit (`verifier_sha`),
shared workflow commit, image digest, binary path, and both checksums. Artifacts
expire after seven days. Candidate tags include the run ID, attempt, and source
short SHA; extraction uses the immutable image digest.

The requested source ref is resolved once. Both builds use recipe files from
Zones' trusted workflow commit, which can differ from the source commit. Keep the
caller's `build-definition-paths` list complete if recipes gain new helpers.
Depot OIDC must allow the Zones caller, and its GitHub token needs write access to
the candidate GHCR package.

This checks the reproducible-profile candidate's binary only. It does not verify
the normal profiling image, the full container filesystem, or the prover EIF.

## Prover EIF

The separate `reproducible_eif_verify` dispatch option compares unsigned prover
EIFs built through Depot and independently on a fresh GitHub runner without Docker
cache. The shared workflow is in `tempoxyz/gh-actions`; Zones supplies the enclave
Dockerfile, `scripts/reproducible-eif-build.sh`, genesis input and Nitro toolchain.

Pin the exact Tempo genesis bytes before dispatching. For example:

```sh
genesis_url=https://devnet-assets.tempoxyz.dev/tempo-devnet-nextfork.json
curl --proto '=https' --proto-redir '=https' --fail --location \
  "$genesis_url" -o /tmp/prover-genesis.json
genesis_sha256=$(sha256sum /tmp/prover-genesis.json | cut -d' ' -f1)
gh workflow run docker.yml --repo tempoxyz/zones --ref main \
  -f reproducible_eif_verify=true -f ref=<source-commit-sha> \
  -f tempo_genesis_url="$genesis_url" -f tempo_genesis_sha256="$genesis_sha256"
```

Each build downloads that URL and rejects bytes that do not match the checksum.
The source ref resolves once; both builds use recipes from the trusted workflow
commit, the reproducible Cargo profile, pinned Rust/Debian images and normalized
rootfs timestamps. The source must contain the reproducible Cargo profile.

By default, the first job builds the existing Nitro toolchain recipe and publishes
a run-specific image under `ghcr.io/tempoxyz/tempo-zone-eif-toolchain`. It records
the resulting digest and uses that exact image for both EIF builds and measurement.
This fixes Nitro CLI, Linux 6.6.79, NSM and bootstrap binaries as toolchain inputs;
the comparison does not independently rebuild or verify the toolchain itself.
To reuse a previous run's toolchain, pass its `eif_builder_image` digest as the
`eif_builder_image` dispatch input. Use the same source and genesis checksum;
if the recipes have changed, reproduce locally with the recorded recipe revision
as described below. Keep the toolchain image available for later reproduction.

Inspect the result after all jobs finish:

```sh
gh run watch <run-id> --repo tempoxyz/zones --exit-status
gh run download <run-id> --repo tempoxyz/zones \
  -n reproducible-prover-eif-verification -D eif-verification
jq -e '.comparison_result == "success" and
       .builds.depot.measurements == .builds.docker.measurements' \
  eif-verification/manifest.json
```

Success requires valid, unsigned EIFs with identical PCR0/PCR1/PCR2. These
measurements are outputs, not input pins. The manifest records both EIF SHA-256
values and `byte_identical` separately; unmeasured EIF metadata can differ without
changing the PCRs. It also records the source, trusted recipe and shared workflow
commits, toolchain digest and genesis checksum. The two raw `describe-eif` reports
are included for inspection.

Artifacts `reproducible-prover-eif-verification-depot` and
`reproducible-prover-eif-verification-docker` contain the actual `enclave.eif`
files. All verification artifacts expire after seven days. A skipped job is not
verification. This mode verifies these candidate EIF artifacts; it does not
establish that a production host image contains the same EIF or perform runtime
Nitro attestation. Normal image publishing is skipped for the verification dispatch.

For a local rebuild on Linux x86-64 with Docker/Buildx, Git, jq and registry access,
check out `source_sha`, initialize submodules, and overlay the three files listed
in `build_definition_paths` from `build_definitions_sha`. Download the genesis
file and verify its recorded SHA-256, then run:

```sh
EIF_BUILDER_IMAGE=$(jq -r .eif_builder_image eif-verification/manifest.json) \
BUILD_INPUT_FILE=/tmp/prover-genesis.json \
NO_CACHE=1 OUT_DIR=/tmp/prover-eif scripts/reproducible-eif-build.sh
docker run --rm --platform linux/amd64 --network none \
  -v /tmp/prover-eif:/input:ro \
  "$(jq -r .eif_builder_image eif-verification/manifest.json)" \
  describe-eif --eif-path /input/enclave.eif
```
